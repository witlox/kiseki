"""E2E: control plane — create organization via gRPC ControlService."""

from __future__ import annotations

import sys
from pathlib import Path

import grpc
import pytest

sys.path.insert(0, str(Path(__file__).parent / "proto"))

from helpers.cluster import ServerInfo
from kiseki.v1 import common_pb2, control_pb2, control_pb2_grpc


@pytest.mark.e2e
def test_create_and_list_organization(kiseki_server: ServerInfo) -> None:
    """Create an organization via ControlService, then list it."""
    channel = grpc.insecure_channel(kiseki_server.data_addr)
    stub = control_pb2_grpc.ControlServiceStub(channel)

    # Create an organization.
    resp = stub.CreateOrganization(
        control_pb2.CreateOrganizationRequest(
            name="org-e2e-test",
            compliance_tags=[1, 2],  # HIPAA, GDPR
            quota=common_pb2.Quota(
                capacity_bytes=500_000_000_000_000,
                iops=100_000,
                metadata_ops_per_sec=10_000,
            ),
            dedup_policy=1,  # cross-tenant
        )
    )
    assert resp.org_id is not None
    assert resp.org_id.value != ""

    # List organizations — should include the one we created.
    list_resp = stub.ListOrganizations(control_pb2.ListOrganizationsRequest())
    assert len(list_resp.organizations) >= 1

    org_names = [o.name for o in list_resp.organizations]
    assert "org-e2e-test" in org_names

    channel.close()


@pytest.mark.e2e
def test_get_organization(kiseki_server: ServerInfo) -> None:
    """Create and retrieve an organization by ID."""
    channel = grpc.insecure_channel(kiseki_server.data_addr)
    stub = control_pb2_grpc.ControlServiceStub(channel)

    # Create.
    create_resp = stub.CreateOrganization(
        control_pb2.CreateOrganizationRequest(
            name="org-get-test",
            quota=common_pb2.Quota(
                capacity_bytes=100_000_000_000_000,
                iops=50_000,
                metadata_ops_per_sec=5_000,
            ),
        )
    )
    org_id = create_resp.org_id.value

    # Get.
    org = stub.GetOrganization(
        control_pb2.GetOrganizationRequest(
            org_id=common_pb2.OrgId(value=org_id),
        )
    )
    assert org.name == "org-get-test"
    assert org.org_id.value == org_id

    channel.close()


@pytest.mark.e2e
def test_create_project_within_org(kiseki_server: ServerInfo) -> None:
    """Create a project within an organization, validating quota."""
    channel = grpc.insecure_channel(kiseki_server.data_addr)
    stub = control_pb2_grpc.ControlServiceStub(channel)

    # Create org.
    org_resp = stub.CreateOrganization(
        control_pb2.CreateOrganizationRequest(
            name="org-project-test",
            quota=common_pb2.Quota(
                capacity_bytes=500_000_000_000_000,
                iops=100_000,
                metadata_ops_per_sec=10_000,
            ),
        )
    )
    org_id = org_resp.org_id.value

    # Create project within org.
    proj_resp = stub.CreateProject(
        control_pb2.CreateProjectRequest(
            org_id=common_pb2.OrgId(value=org_id),
            name="proj-clinical",
            quota=common_pb2.Quota(
                capacity_bytes=200_000_000_000_000,
                iops=50_000,
                metadata_ops_per_sec=5_000,
            ),
        )
    )
    assert proj_resp.project_id is not None
    assert proj_resp.project_id.value != ""

    channel.close()
