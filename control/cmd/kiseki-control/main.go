// Kiseki control-plane API server entry point.
//
// Runs the ControlService and AuditExportService gRPC servers on the
// management network.
package main

import (
	"fmt"
	"log"
	"net"
	"os"
	"os/signal"
	"syscall"

	controlgrpc "github.com/witlox/kiseki/control/pkg/grpc"
	"github.com/witlox/kiseki/control/pkg/tenant"
	"github.com/witlox/kiseki/control/pkg/version"
	pb "github.com/witlox/kiseki/control/proto/kiseki/v1"
	"google.golang.org/grpc"
)

func main() {
	fmt.Fprintf(os.Stdout,
		"kiseki-control %s (commit %s, built %s)\n",
		version.Version, version.Commit, version.BuildTime,
	)

	addr := os.Getenv("KISEKI_CONTROL_ADDR")
	if addr == "" {
		addr = "0.0.0.0:9200"
	}

	lis, err := net.Listen("tcp", addr)
	if err != nil {
		log.Fatalf("failed to listen on %s: %v", addr, err)
	}

	tenantStore := tenant.NewStore()

	// TODO(auth): Add mTLS from KISEKI_CA_PATH/KISEKI_CERT_PATH/KISEKI_KEY_PATH
	// matching the data-path server pattern. Currently plaintext — must be
	// enforced before any networked deployment. See: I-T4, I-Auth1.
	srv := grpc.NewServer()
	pb.RegisterControlServiceServer(srv, controlgrpc.NewControlServer(tenantStore))
	pb.RegisterAuditExportServiceServer(srv, controlgrpc.NewAuditServer())

	go func() {
		sigCh := make(chan os.Signal, 1)
		signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
		<-sigCh
		log.Println("shutting down control plane...")
		srv.GracefulStop()
	}()

	log.Printf("control-plane gRPC listening on %s (plaintext)", addr)
	if err := srv.Serve(lis); err != nil {
		log.Fatalf("gRPC serve: %v", err)
	}
}
