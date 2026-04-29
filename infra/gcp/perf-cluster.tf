terraform {
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 5.0"
    }
  }
}

provider "google" {
  project = var.project_id
  region  = var.region
  zone    = var.zone
}

# ============================================================================
# Variables
# ============================================================================

variable "project_id" {
  description = "GCP project ID"
  type        = string
}

variable "region" {
  type    = string
  default = "europe-west6"
}

variable "zone" {
  type    = string
  default = "europe-west6-a"
}

variable "release_tag" {
  description = "GitHub release tag for kiseki binaries (e.g. 'v2026.39.501' or 'latest')"
  type        = string
  default     = "latest"
}

variable "profile" {
  description = <<-EOT
    Cluster profile — selects the shape *and* which benchmark suite the ctrl runs:

      "default"   — broad coverage. 6 × c3-standard-22 storage with 4 × local NVMe each
                    (1.5 TB / node), 3 × c3-standard-22 client. 6 nodes ≥ EC-4+2 minimum,
                    pure-NVMe device pool, Tier_1 50 Gbps. Runs perf-suite.sh.
      "transport" — protocol/NIC ceiling. 3 × c3-standard-88 storage with 8 × local NVMe
                    each (3 TB / node), 3 × c3-standard-44 client. Tier_1 100 Gbps, disks
                    deliberately faster than the wire so any cap is gateway/grpc, not I/O.
                    Runs perf-suite-transport.sh.
      "gpu"       — ML training scenario. 3 × c3-standard-44 storage (4 × local NVMe,
                    Tier_1 50 Gbps), 2 × a2-highgpu-1g GPU clients (1 × A100 each).
                    Runs perf-suite-gpu.sh against the cuFile/GDS path.
  EOT
  type        = string
  default     = "default"
  validation {
    condition     = contains(["default", "transport", "gpu"], var.profile)
    error_message = "profile must be one of: default, transport, gpu"
  }
}

# ============================================================================
# Profile shapes — every per-profile choice lives in this map
# ============================================================================

locals {
  profiles = {
    default = {
      label              = "default"
      storage_count      = 6
      storage_machine    = "c3-standard-22"
      storage_local_ssds = 4
      storage_tier_1     = true
      client_count       = 3
      client_machine     = "c3-standard-22"
      client_cache_gb    = 200
      client_tier_1      = true
      client_gpu         = false
      client_image       = "rocky-linux-cloud/rocky-linux-9"
      client_setup       = "setup-perf-client.sh"
      bench_suite        = "perf-suite.sh"
    }
    transport = {
      label              = "transport"
      storage_count      = 3
      storage_machine    = "c3-standard-88"
      storage_local_ssds = 8
      storage_tier_1     = true
      client_count       = 3
      client_machine     = "c3-standard-44"
      client_cache_gb    = 100
      client_tier_1      = true
      client_gpu         = false
      client_image       = "rocky-linux-cloud/rocky-linux-9"
      client_setup       = "setup-perf-client.sh"
      bench_suite        = "perf-suite-transport.sh"
    }
    gpu = {
      label              = "gpu"
      storage_count      = 3
      storage_machine    = "c3-standard-44"
      storage_local_ssds = 4
      storage_tier_1     = true
      client_count       = 2
      client_machine     = "a2-highgpu-1g"
      client_cache_gb    = 1000
      client_tier_1      = false # a2-highgpu-1g (12 vCPU) is below the Tier_1 floor
      client_gpu         = true
      # Google's Deep Learning VM image: Debian 11 + CUDA 12.3 + nvidia drivers preinstalled.
      client_image = "deeplearning-platform-release/common-cu123-debian-11"
      client_setup = "setup-gpu-client.sh"
      bench_suite  = "perf-suite-gpu.sh"
    }
  }

  p = local.profiles[var.profile]

  # Local SSDs appear under stable by-id symlinks regardless of NVMe namespace ordering.
  raw_devices = join(",", [for i in range(local.p.storage_local_ssds) : "/dev/disk/by-id/google-local-ssd-${i}"])

  storage_ips = [for i in range(local.p.storage_count) : "10.0.0.${10 + i}"]
  client_ips  = [for i in range(local.p.client_count) : "10.0.0.${30 + i}"]

  raft_port = 9300
  all_peers = join(",", [for i, ip in local.storage_ips : "${i + 1}=${ip}:${local.raft_port}"])
}

# ============================================================================
# Network
# ============================================================================

resource "google_compute_network" "net" {
  name                    = "kiseki-perf-${local.p.label}"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "sub" {
  name          = "kiseki-perf-${local.p.label}-sub"
  ip_cidr_range = "10.0.0.0/24"
  network       = google_compute_network.net.id
  region        = var.region
}

resource "google_compute_firewall" "internal" {
  name    = "kiseki-perf-${local.p.label}-internal"
  network = google_compute_network.net.name
  allow {
    protocol = "tcp"
    ports    = ["0-65535"]
  }
  allow {
    protocol = "udp"
    ports    = ["0-65535"]
  }
  allow {
    protocol = "icmp"
  }
  source_ranges = ["10.0.0.0/24"]
}

resource "google_compute_firewall" "ssh" {
  name    = "kiseki-perf-${local.p.label}-ssh"
  network = google_compute_network.net.name
  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
  source_ranges = ["0.0.0.0/0"]
}

resource "google_compute_firewall" "services" {
  name    = "kiseki-perf-${local.p.label}-svc"
  network = google_compute_network.net.name
  allow {
    protocol = "tcp"
    ports    = ["2049", "9000", "9090", "9100", "9101", "9102"]
  }
  source_ranges = ["0.0.0.0/0"]
  target_tags   = ["kiseki-storage"]
}

# ============================================================================
# Storage nodes
# ----------------------------------------------------------------------------
# N × <profile machine> with M × local NVMe SSD as RAW block devices.
# Disks are NOT mounted — Kiseki DeviceBackend manages them directly.
# ============================================================================

resource "google_compute_instance" "storage" {
  count        = local.p.storage_count
  name         = "kiseki-storage-${count.index + 1}"
  machine_type = local.p.storage_machine
  zone         = var.zone
  tags         = ["kiseki-storage"]

  boot_disk {
    initialize_params {
      image = "rocky-linux-cloud/rocky-linux-9"
      size  = 50
      type  = "pd-ssd"
    }
  }

  dynamic "scratch_disk" {
    for_each = range(local.p.storage_local_ssds)
    content {
      interface = "NVME"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.sub.id
    network_ip = local.storage_ips[count.index]
    nic_type   = "GVNIC"
    access_config {}
  }

  dynamic "network_performance_config" {
    for_each = local.p.storage_tier_1 ? [1] : []
    content {
      total_egress_bandwidth_tier = "TIER_1"
    }
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-raw-storage.sh", {
    node_id      = count.index + 1
    node_ip      = local.storage_ips[count.index]
    all_peers    = local.all_peers
    raft_port    = local.raft_port
    raw_devices  = local.raw_devices
    device_class = "nvme"
    meta_dir     = "/var/lib/kiseki"
    release_tag  = var.release_tag
  })
}

# ============================================================================
# Client nodes
# ----------------------------------------------------------------------------
# Profile-determined: CPU clients for default/transport, GPU clients for gpu.
# ============================================================================

resource "google_compute_instance" "client" {
  count        = local.p.client_count
  name         = "kiseki-client-${count.index + 1}"
  machine_type = local.p.client_machine
  zone         = var.zone

  boot_disk {
    initialize_params {
      image = local.p.client_image
      size  = local.p.client_gpu ? 100 : 50
      type  = "pd-ssd"
    }
  }

  attached_disk {
    source      = google_compute_disk.cache[count.index].id
    device_name = "kiseki-cache"
    mode        = "READ_WRITE"
  }

  dynamic "guest_accelerator" {
    for_each = local.p.client_gpu ? [1] : []
    content {
      type  = "nvidia-tesla-a100"
      count = 1
    }
  }

  scheduling {
    on_host_maintenance = local.p.client_gpu ? "TERMINATE" : "MIGRATE"
    automatic_restart   = true
  }

  network_interface {
    subnetwork = google_compute_subnetwork.sub.id
    network_ip = local.client_ips[count.index]
    nic_type   = "GVNIC"
    access_config {}
  }

  dynamic "network_performance_config" {
    for_each = local.p.client_tier_1 ? [1] : []
    content {
      total_egress_bandwidth_tier = "TIER_1"
    }
  }

  metadata = {
    enable-oslogin        = "TRUE"
    install-nvidia-driver = local.p.client_gpu ? "True" : null
  }

  metadata_startup_script = templatefile("${path.module}/scripts/${local.p.client_setup}", {
    storage_ips = join(",", local.storage_ips)
    cache_dev   = "/dev/disk/by-id/google-kiseki-cache"
    client_id   = count.index + 1
    release_tag = var.release_tag
    profile     = local.p.label
  })
}

resource "google_compute_disk" "cache" {
  count = local.p.client_count
  name  = "kiseki-cache-${count.index}"
  type  = "pd-ssd"
  size  = local.p.client_cache_gb
  zone  = var.zone
}

# ============================================================================
# Results bucket + ctrl service account
# ============================================================================

resource "google_storage_bucket" "perf_results" {
  name          = "${var.project_id}-kiseki-perf-${local.p.label}"
  location      = var.region
  force_destroy = true

  lifecycle_rule {
    condition {
      age = 30
    }
    action {
      type = "Delete"
    }
  }

  uniform_bucket_level_access = true
}

resource "google_service_account" "ctrl" {
  account_id   = "kiseki-bench-${local.p.label}"
  display_name = "Kiseki benchmark controller (${local.p.label})"
}

resource "google_project_iam_member" "ctrl_os_login" {
  project = var.project_id
  role    = "roles/compute.osAdminLogin"
  member  = "serviceAccount:${google_service_account.ctrl.email}"
}

resource "google_project_iam_member" "ctrl_sa_user" {
  project = var.project_id
  role    = "roles/iam.serviceAccountUser"
  member  = "serviceAccount:${google_service_account.ctrl.email}"
}

resource "google_storage_bucket_iam_member" "ctrl_write" {
  bucket = google_storage_bucket.perf_results.name
  role   = "roles/storage.objectCreator"
  member = "serviceAccount:${google_service_account.ctrl.email}"
}

resource "google_storage_bucket_iam_member" "ctrl_read" {
  bucket = google_storage_bucket.perf_results.name
  role   = "roles/storage.objectViewer"
  member = "serviceAccount:${google_service_account.ctrl.email}"
}

# ============================================================================
# Benchmark controller
# ============================================================================

resource "google_compute_instance" "ctrl" {
  name         = "kiseki-ctrl"
  machine_type = "e2-standard-4"
  zone         = var.zone

  boot_disk {
    initialize_params {
      image = "rocky-linux-cloud/rocky-linux-9"
      size  = 50
      type  = "pd-balanced"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.sub.id
    network_ip = "10.0.0.100"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  service_account {
    email  = google_service_account.ctrl.email
    scopes = ["cloud-platform"]
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-bench-ctrl.sh", {
    storage_ips = join(",", local.storage_ips)
    client_ips  = join(",", local.client_ips)
    perf_bucket = "gs://${google_storage_bucket.perf_results.name}"
    release_tag = var.release_tag
    profile     = local.p.label
    bench_suite = local.p.bench_suite
  })
}

# ============================================================================
# Outputs
# ============================================================================

output "profile" {
  value = local.p.label
}

output "storage_nodes" {
  value = [for i in google_compute_instance.storage : {
    name = i.name
    ip   = i.network_interface[0].access_config[0].nat_ip
    int  = i.network_interface[0].network_ip
  }]
}

output "clients" {
  value = [for i in google_compute_instance.client : {
    name = i.name
    ip   = i.network_interface[0].access_config[0].nat_ip
    int  = i.network_interface[0].network_ip
  }]
}

output "ctrl_ip" {
  value = google_compute_instance.ctrl.network_interface[0].access_config[0].nat_ip
}

output "perf_bucket" {
  value = "gs://${google_storage_bucket.perf_results.name}"
}

output "bench_suite" {
  value = local.p.bench_suite
}

output "dashboard" {
  value = "http://${google_compute_instance.storage[0].network_interface[0].access_config[0].nat_ip}:9090/ui"
}
