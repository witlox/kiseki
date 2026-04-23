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
  description = "GCP region"
  type        = string
  default     = "europe-west6" # Zurich — close to CSCS
}

variable "zone" {
  description = "GCP zone"
  type        = string
  default     = "europe-west6-a"
}

variable "ssh_user" {
  description = "SSH username (used with OS Login)"
  type        = string
  default     = "kiseki"
}

# ---------------------------------------------------------------------------
# Network
# ---------------------------------------------------------------------------

resource "google_compute_network" "kiseki" {
  name                    = "kiseki-perf-test"
  auto_create_subnetworks = false
}

resource "google_compute_subnetwork" "kiseki" {
  name          = "kiseki-perf-subnet"
  ip_cidr_range = "10.0.0.0/24"
  network       = google_compute_network.kiseki.id
  region        = var.region
}

resource "google_compute_firewall" "internal" {
  name    = "kiseki-internal"
  network = google_compute_network.kiseki.name

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
  name    = "kiseki-ssh"
  network = google_compute_network.kiseki.name

  allow {
    protocol = "tcp"
    ports    = ["22"]
  }

  source_ranges = ["0.0.0.0/0"]
}

resource "google_compute_firewall" "web" {
  name    = "kiseki-web"
  network = google_compute_network.kiseki.name

  allow {
    protocol = "tcp"
    ports    = ["9000", "9090", "9100", "2049"]
  }

  source_ranges = ["0.0.0.0/0"]
  target_tags   = ["kiseki-storage"]
}

# ---------------------------------------------------------------------------
# Storage nodes (3 nodes, different disk types)
# ---------------------------------------------------------------------------

# Storage node 1: Local NVMe SSD (best latency)
resource "google_compute_instance" "storage_1" {
  name         = "kiseki-storage-1"
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

  # Local NVMe SSDs for data
  scratch_disk {
    interface = "NVME"
  }
  scratch_disk {
    interface = "NVME"
  }

  network_interface {
    subnetwork = google_compute_subnetwork.kiseki.id
    network_ip = "10.0.0.10"
    access_config {} # ephemeral public IP
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-storage.sh", {
    node_id       = 1
    node_ip       = "10.0.0.10"
    peer_ips      = "1=10.0.0.10:9300,2=10.0.0.11:9300,3=10.0.0.12:9300"
    data_dir      = "/data"
    disk_type     = "local-nvme"
    metrics_port  = 9090
  })
}

# Storage node 2: PD-SSD (standard network-attached SSD)
resource "google_compute_instance" "storage_2" {
  name         = "kiseki-storage-2"
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

  attached_disk {
    source      = google_compute_disk.data_ssd.id
    device_name = "kiseki-data"
  }

  network_interface {
    subnetwork = google_compute_subnetwork.kiseki.id
    network_ip = "10.0.0.11"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-storage.sh", {
    node_id       = 2
    node_ip       = "10.0.0.11"
    peer_ips      = "1=10.0.0.10:9300,2=10.0.0.11:9300,3=10.0.0.12:9300"
    data_dir      = "/data"
    disk_type     = "pd-ssd"
    metrics_port  = 9090
  })
}

resource "google_compute_disk" "data_ssd" {
  name = "kiseki-data-ssd"
  type = "pd-ssd"
  size = 500
  zone = var.zone
}

# Storage node 3: PD-Balanced (cost baseline)
resource "google_compute_instance" "storage_3" {
  name         = "kiseki-storage-3"
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

  attached_disk {
    source      = google_compute_disk.data_balanced.id
    device_name = "kiseki-data"
  }

  network_interface {
    subnetwork = google_compute_subnetwork.kiseki.id
    network_ip = "10.0.0.12"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-storage.sh", {
    node_id       = 3
    node_ip       = "10.0.0.12"
    peer_ips      = "1=10.0.0.10:9300,2=10.0.0.11:9300,3=10.0.0.12:9300"
    data_dir      = "/data"
    disk_type     = "pd-balanced"
    metrics_port  = 9090
  })
}

resource "google_compute_disk" "data_balanced" {
  name = "kiseki-data-balanced"
  type = "pd-balanced"
  size = 500
  zone = var.zone
}

# ---------------------------------------------------------------------------
# Client nodes (3 nodes, different workload profiles)
# ---------------------------------------------------------------------------

# Client 1: NVMe cache, S3 benchmark
resource "google_compute_instance" "client_1" {
  name         = "kiseki-client-1"
  machine_type = "n2-standard-8"
  zone         = var.zone

  boot_disk {
    initialize_params {
      image = "rocky-linux-cloud/rocky-linux-9"
      size  = 50
      type  = "pd-ssd"
    }
  }

  attached_disk {
    source      = google_compute_disk.client_cache_1.id
    device_name = "kiseki-cache"
  }

  network_interface {
    subnetwork = google_compute_subnetwork.kiseki.id
    network_ip = "10.0.0.20"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-client.sh", {
    storage_ips = "10.0.0.10,10.0.0.11,10.0.0.12"
    cache_dir   = "/cache"
    role        = "s3-bench"
  })
}

resource "google_compute_disk" "client_cache_1" {
  name = "kiseki-cache-1"
  type = "pd-ssd"
  size = 100
  zone = var.zone
}

# Client 2: GPU node (T4) for GPU-direct testing
resource "google_compute_instance" "client_2" {
  name         = "kiseki-client-2"
  machine_type = "n1-standard-8"
  zone         = var.zone

  boot_disk {
    initialize_params {
      image = "rocky-linux-cloud/rocky-linux-9"
      size  = 50
      type  = "pd-ssd"
    }
  }

  # GPU accelerator removed — T4 not available in europe-west6-a.
  # For GPU-direct testing, use a zone with GPU availability.

  network_interface {
    subnetwork = google_compute_subnetwork.kiseki.id
    network_ip = "10.0.0.21"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-client.sh", {
    storage_ips = "10.0.0.10,10.0.0.11,10.0.0.12"
    cache_dir   = "/cache"
    role        = "gpu-bench"
  })
}

# Client 3: NFS + FUSE mount testing
resource "google_compute_instance" "client_3" {
  name         = "kiseki-client-3"
  machine_type = "n2-standard-8"
  zone         = var.zone

  boot_disk {
    initialize_params {
      image = "rocky-linux-cloud/rocky-linux-9"
      size  = 50
      type  = "pd-ssd"
    }
  }

  network_interface {
    subnetwork = google_compute_subnetwork.kiseki.id
    network_ip = "10.0.0.22"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-client.sh", {
    storage_ips = "10.0.0.10,10.0.0.11,10.0.0.12"
    cache_dir   = "/cache"
    role        = "nfs-fuse-bench"
  })
}

# ---------------------------------------------------------------------------
# Benchmark controller
# ---------------------------------------------------------------------------

resource "google_compute_instance" "bench_ctrl" {
  name         = "bench-ctrl"
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
    subnetwork = google_compute_subnetwork.kiseki.id
    network_ip = "10.0.0.100"
    access_config {}
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/scripts/setup-bench-ctrl.sh", {
    storage_ips = "10.0.0.10,10.0.0.11,10.0.0.12"
    client_ips  = "10.0.0.20,10.0.0.21,10.0.0.22"
  })
}

# ---------------------------------------------------------------------------
# Outputs
# ---------------------------------------------------------------------------

output "storage_ips" {
  value = {
    storage_1 = google_compute_instance.storage_1.network_interface[0].access_config[0].nat_ip
    storage_2 = google_compute_instance.storage_2.network_interface[0].access_config[0].nat_ip
    storage_3 = google_compute_instance.storage_3.network_interface[0].access_config[0].nat_ip
  }
}

output "client_ips" {
  value = {
    client_1 = google_compute_instance.client_1.network_interface[0].access_config[0].nat_ip
    client_2 = google_compute_instance.client_2.network_interface[0].access_config[0].nat_ip
    client_3 = google_compute_instance.client_3.network_interface[0].access_config[0].nat_ip
  }
}

output "bench_ctrl_ip" {
  value = google_compute_instance.bench_ctrl.network_interface[0].access_config[0].nat_ip
}

output "dashboard_url" {
  value = "http://${google_compute_instance.storage_1.network_interface[0].access_config[0].nat_ip}:9090/ui"
}
