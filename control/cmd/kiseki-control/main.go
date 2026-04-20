// Kiseki control-plane API server entry point.
//
// Runs the ControlService and AuditExportService gRPC servers on the
// management network. Supports optional mTLS via KISEKI_CA_PATH,
// KISEKI_CERT_PATH, KISEKI_KEY_PATH environment variables.
package main

import (
	"crypto/tls"
	"crypto/x509"
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
	"google.golang.org/grpc/credentials"
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

	var opts []grpc.ServerOption
	if tlsCreds, ok := loadTLS(); ok {
		opts = append(opts, grpc.Creds(tlsCreds))
		log.Printf("control-plane gRPC listening on %s (mTLS)", addr)
	} else {
		log.Printf("WARNING: control-plane gRPC listening on %s (plaintext — development only)", addr)
	}

	srv := grpc.NewServer(opts...)
	pb.RegisterControlServiceServer(srv, controlgrpc.NewControlServer(tenantStore))
	pb.RegisterAuditExportServiceServer(srv, controlgrpc.NewAuditServer())

	go func() {
		sigCh := make(chan os.Signal, 1)
		signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
		<-sigCh
		log.Println("shutting down control plane...")
		srv.GracefulStop()
	}()

	if err := srv.Serve(lis); err != nil {
		log.Fatalf("gRPC serve: %v", err)
	}
}

// loadTLS builds mTLS credentials from environment variables.
// Returns (creds, true) if all three files are set, (nil, false) otherwise.
func loadTLS() (credentials.TransportCredentials, bool) {
	caPath := os.Getenv("KISEKI_CA_PATH")
	certPath := os.Getenv("KISEKI_CERT_PATH")
	keyPath := os.Getenv("KISEKI_KEY_PATH")

	if caPath == "" || certPath == "" || keyPath == "" {
		return nil, false
	}

	caPem, err := os.ReadFile(caPath)
	if err != nil {
		log.Printf("WARNING: TLS CA read failed: %v — falling back to plaintext", err)
		return nil, false
	}

	certPem, err := tls.LoadX509KeyPair(certPath, keyPath)
	if err != nil {
		log.Printf("WARNING: TLS cert/key load failed: %v — falling back to plaintext", err)
		return nil, false
	}

	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(caPem) {
		log.Printf("WARNING: no valid CA certs found — falling back to plaintext")
		return nil, false
	}

	tlsConfig := &tls.Config{
		Certificates: []tls.Certificate{certPem},
		ClientAuth:   tls.RequireAndVerifyClientCert,
		ClientCAs:    pool,
		MinVersion:   tls.VersionTLS13,
	}

	return credentials.NewTLS(tlsConfig), true
}
