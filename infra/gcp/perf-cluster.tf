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
  description = "GitHub release tag to download binaries from (e.g. 'latest' or 'tags/v2026.1.42')"
  type        = string
  default     = "latest"
}

# ---------------------------------------------------------------------------
# Network
# ---------------------------------------------------------------------------

resource "google_compute_network" "net" {
  name                    = "kiseki-perf"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "sub" {
  name          = "kiseki-perf-sub"
  ip_cidr_range = "10.0.0.0/24"
  network       = google_compute_network.net.id
  region        = var.region
}

resource "google_compute_firewall" "internal" {
  name    = "kiseki-perf-internal"
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
  name    = "kiseki-perf-ssh"
  network = google_compute_network.net.name
  allow {
    protocol = "tcp"
    ports    = ["22"]
  }
  source_ranges = ["0.0.0.0/0"]
}

resource "google_compute_firewall" "services" {
  name    = "kiseki-perf-svc"
  network = google_compute_network.net.name
  allow {
    protocol = "tcp"
    ports    = ["2049", "9000", "9090", "9100", "9101", "9102"]
  }
  source_ranges = ["0.0.0.0/0"]
  target_tags   = ["kiseki-storage"]
}

# ---------------------------------------------------------------------------
# HDD nodes: 3 × n2-standard-16, each with 3 × PD-Standard (HDD perf)
# Disks are raw block devices — NOT mounted. Kiseki DeviceBackend manages them.
# ---------------------------------------------------------------------------

resource "google_compute_instance" "hdd" {
  count        = 3
  name         = "kiseki-hdd-${count.index + 1}"
  machine_type = "n2-standard-16"
  zone         = var.zone
  tags         = ["kiseki-storage"]

  boot_disk {
    initialize_params {
      image = "rocky-linux-cloud/rocky-linux-9"
      size  = 50
      type  = "pd-ssd"
    }
  }

  # 3 × PD-Standard (HDD performance tier) — raw block, no filesystem
  dynamic "attached_disk" {
    for_each = range(3)
    content {
      source      = google_compute_disk.hdd[count.index * 3 + attached_disk.value].id
      device_name = "kiseki-hdd-${attached_disk.value}"
      mode        = "READ_WRITE"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.sub.id
    network_ip = "10.0.0.${10 + count.index}"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-raw-storage.sh", {
    node_id   = count.index + 1
    node_ip   = "10.0.0.${10 + count.index}"
    all_peers = "1=10.0.0.10:9300,2=10.0.0.11:9300,3=10.0.0.12:9300,4=10.0.0.20:9300,5=10.0.0.21:9300"
    raft_port = 9300
    # Raw device paths — Kiseki manages these directly
    raw_devices  = "/dev/disk/by-id/google-kiseki-hdd-0,/dev/disk/by-id/google-kiseki-hdd-1,/dev/disk/by-id/google-kiseki-hdd-2"
    device_class = "hdd"
    meta_dir     = "/var/lib/kiseki"
    release_tag  = var.release_tag
  })
}

# 9 PD-Standard disks (3 per HDD node)
resource "google_compute_disk" "hdd" {
  count = 9
  name  = "kiseki-data-hdd-${count.index}"
  type  = "pd-standard"
  size  = 200 # 200GB each, 600GB per node
  zone  = var.zone
}

# ---------------------------------------------------------------------------
# Fast nodes: 2 × n2-standard-16, each with 1 × local NVMe + 2 × PD-SSD
# NVMe = metadata tier, PD-SSD = data tier — all raw block
# ---------------------------------------------------------------------------

resource "google_compute_instance" "fast" {
  count        = 2
  name         = "kiseki-fast-${count.index + 1}"
  machine_type = "n2-standard-16"
  zone         = var.zone
  tags         = ["kiseki-storage"]

  boot_disk {
    initialize_params {
      image = "rocky-linux-cloud/rocky-linux-9"
      size  = 50
      type  = "pd-ssd"
    }
  }

  # 2 × local NVMe SSD (GCP requires multiples of 2)
  scratch_disk {
    interface = "NVME"
  }
  scratch_disk {
    interface = "NVME"
  }

  # 2 × PD-SSD (data tier)
  dynamic "attached_disk" {
    for_each = range(2)
    content {
      source      = google_compute_disk.ssd[count.index * 2 + attached_disk.value].id
      device_name = "kiseki-ssd-${attached_disk.value}"
      mode        = "READ_WRITE"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.sub.id
    network_ip = "10.0.0.${20 + count.index}"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-raw-storage.sh", {
    node_id   = count.index + 4
    node_ip   = "10.0.0.${20 + count.index}"
    all_peers = "1=10.0.0.10:9300,2=10.0.0.11:9300,3=10.0.0.12:9300,4=10.0.0.20:9300,5=10.0.0.21:9300"
    raft_port = 9300
    # NVMe for metadata, PD-SSD for data — all raw
    raw_devices  = "/dev/nvme0n1,/dev/disk/by-id/google-kiseki-ssd-0,/dev/disk/by-id/google-kiseki-ssd-1"
    device_class = "nvme+ssd"
    meta_dir     = "/var/lib/kiseki"
    release_tag  = var.release_tag
  })
}

# 4 PD-SSD disks (2 per fast node)
resource "google_compute_disk" "ssd" {
  count = 4
  name  = "kiseki-ssd-${count.index}"
  type  = "pd-ssd"
  size  = 375 # 375GB each, 750GB per node
  zone  = var.zone
}

# ---------------------------------------------------------------------------
# Client nodes: 3 × n2-standard-8 for FUSE + NFS + benchmark
# ---------------------------------------------------------------------------

resource "google_compute_instance" "client" {
  count        = 3
  name         = "kiseki-client-${count.index + 1}"
  machine_type = "n2-standard-8"
  zone         = var.zone

  boot_disk {
    initialize_params {
      image = "rocky-linux-cloud/rocky-linux-9"
      size  = 50
      type  = "pd-ssd"
    }
  }

  # Client cache disk (L2)
  attached_disk {
    source      = google_compute_disk.cache[count.index].id
    device_name = "kiseki-cache"
    mode        = "READ_WRITE"
  }

  network_interface {
    subnetwork = google_compute_subnetwork.sub.id
    network_ip = "10.0.0.${30 + count.index}"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-perf-client.sh", {
    storage_ips = "10.0.0.10,10.0.0.11,10.0.0.12,10.0.0.20,10.0.0.21"
    cache_dev   = "/dev/disk/by-id/google-kiseki-cache"
    client_id   = count.index + 1
    release_tag = var.release_tag
  })
}

resource "google_compute_disk" "cache" {
  count = 3
  name  = "kiseki-cache-${count.index}"
  type  = "pd-ssd"
  size  = 100
  zone  = var.zone
}

# ---------------------------------------------------------------------------
# GCS bucket for performance results
# ---------------------------------------------------------------------------

resource "google_storage_bucket" "perf_results" {
  name          = "${var.project_id}-kiseki-perf-results"
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

# Service account for ctrl node (GCS write access)
resource "google_service_account" "ctrl" {
  account_id   = "kiseki-bench-ctrl"
  display_name = "Kiseki benchmark controller"
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

# ---------------------------------------------------------------------------
# Benchmark controller
# ---------------------------------------------------------------------------

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
    storage_ips = "10.0.0.10,10.0.0.11,10.0.0.12,10.0.0.20,10.0.0.21"
    client_ips  = "10.0.0.30,10.0.0.31,10.0.0.32"
    perf_bucket = "gs://${google_storage_bucket.perf_results.name}"
    release_tag = var.release_tag
  })
}

# ---------------------------------------------------------------------------
# Outputs
# ---------------------------------------------------------------------------

output "hdd_nodes" {
  value = [for i in google_compute_instance.hdd : {
    name = i.name
    ip   = i.network_interface[0].access_config[0].nat_ip
    int  = i.network_interface[0].network_ip
  }]
}

output "fast_nodes" {
  value = [for i in google_compute_instance.fast : {
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

output "dashboard" {
  value = "http://${google_compute_instance.fast[0].network_interface[0].access_config[0].nat_ip}:9090/ui"
}
