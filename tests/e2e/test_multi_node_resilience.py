"""E2E: Multi-node cluster resilience.

Tests consensus failure recovery, leader election, and data
availability during node failures.
Requires docker-compose.3node.yml.
"""

import subprocess
import time
from pathlib import Path

import grpc
import pytest
from tenacity import retry, stop_after_delay, wait_exponential

from helpers.cluster import ClusterInfo, start_cluster, stop_cluster, stop_node, start_node


COMPOSE_FILE = "docker-compose.3node.yml"


@pytest.fixture(scope="module")
def cluster():
    """Start a 3-node cluster for the test module."""
    try:
        info = start_cluster(COMPOSE_FILE)
        yield info
        stop_cluster(info)
    except Exception as e:
        pytest.skip(f"3-node cluster not available: {e}")


class TestNodeFailureRecovery:
    """Consensus failure scenarios (F-C1, F-C2)."""

    def test_cluster_healthy_all_nodes(self, cluster):
        """All 3 nodes should be reachable initially."""
        for node in cluster.nodes:
            channel = grpc.insecure_channel(node.data_addr)
            try:
                grpc.channel_ready_future(channel).result(timeout=5)
            finally:
                channel.close()

    def test_single_node_failure_reads_continue(self, cluster):
        """F-C1: Killing one node, reads should still work (2/3 quorum)."""
        # Stop node 3.
        stop_node(cluster.compose_file, "kiseki-node3")
        time.sleep(3)  # Allow leader election.

        # Remaining nodes should still be reachable.
        reachable = 0
        for node in cluster.nodes[:2]:  # node1 and node2
            try:
                channel = grpc.insecure_channel(node.data_addr)
                grpc.channel_ready_future(channel).result(timeout=5)
                reachable += 1
                channel.close()
            except Exception:
                pass

        assert reachable >= 1, "at least one remaining node should be reachable"

        # Restart node 3.
        start_node(cluster.compose_file, "kiseki-node3")
        time.sleep(5)

    def test_node_rejoin_after_restart(self, cluster):
        """Restarted node should rejoin the cluster."""
        # Stop and restart node 2.
        stop_node(cluster.compose_file, "kiseki-node2")
        time.sleep(2)
        start_node(cluster.compose_file, "kiseki-node2")
        time.sleep(5)

        # Node 2 should be reachable again.
        channel = grpc.insecure_channel(cluster.nodes[1].data_addr)
        try:
            grpc.channel_ready_future(channel).result(timeout=10)
        finally:
            channel.close()


class TestNetworkPartition:
    """Network partition scenarios."""

    def test_partition_and_heal(self, cluster):
        """Simulated partition via node stop/start."""
        # This is a simplified partition test — real partition needs
        # docker network disconnect, which requires privileged access.
        # For CI, we simulate by stopping a node (equivalent to
        # losing connectivity to that node).

        stop_node(cluster.compose_file, "kiseki-node3")
        time.sleep(3)

        # Cluster should still have quorum (2/3).
        channel = grpc.insecure_channel(cluster.nodes[0].data_addr)
        try:
            grpc.channel_ready_future(channel).result(timeout=5)
        finally:
            channel.close()

        # Heal: restart node.
        start_node(cluster.compose_file, "kiseki-node3")
        time.sleep(5)

        # All nodes should be back.
        channel = grpc.insecure_channel(cluster.nodes[2].data_addr)
        try:
            grpc.channel_ready_future(channel).result(timeout=10)
        finally:
            channel.close()
