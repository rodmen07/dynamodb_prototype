package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
)

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type HealthResponse struct {
	Status  string `json:"status"`
	Service string `json:"service"`
}

type PipelineCounts struct {
	Bronze  int `json:"bronze"`
	Cleaned int `json:"cleaned"`
	Silver  int `json:"silver"`
	Gold    int `json:"gold"`
}

type ServiceStatus struct {
	Name      string `json:"name"`
	URL       string `json:"url"`
	Status    string `json:"status"` // "ok" | "error"
	LatencyMs int64  `json:"latency_ms"`
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

func requireAdmin(next http.HandlerFunc) http.HandlerFunc {
	adminKey := os.Getenv("ADMIN_KEY")
	if adminKey == "" {
		log.Fatal("ADMIN_KEY must be set")
	}
	return func(w http.ResponseWriter, r *http.Request) {
		provided := r.Header.Get("X-Admin-Key")
		if provided == "" {
			// Also accept Bearer token
			auth := r.Header.Get("Authorization")
			if strings.HasPrefix(auth, "Bearer ") {
				provided = strings.TrimPrefix(auth, "Bearer ")
			}
		}
		if provided != adminKey {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(http.StatusUnauthorized)
			_ = json.NewEncoder(w).Encode(map[string]string{
				"code":    "UNAUTHORIZED",
				"message": "invalid or missing admin key",
			})
			return
		}
		next(w, r)
	}
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

func handleHealth(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(HealthResponse{Status: "ok", Service: "go-pipeline-monitor"})
}

func handlePipeline(ddb *dynamodb.Client) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		table := os.Getenv("DDB_TABLE")
		if table == "" {
			table = "example_table"
		}

		counts := PipelineCounts{}
		stages := map[string]*int{
			"bronze":         &counts.Bronze,
			"bronze_cleaned": &counts.Cleaned,
			"silver":         &counts.Silver,
			"gold":           &counts.Gold,
		}

		var wg sync.WaitGroup
		var mu sync.Mutex

		for stage, counter := range stages {
			wg.Add(1)
			go func(stage string, counter *int) {
				defer wg.Done()
				n := countByStage(r.Context(), ddb, table, stage)
				mu.Lock()
				*counter = n
				mu.Unlock()
			}(stage, counter)
		}
		wg.Wait()

		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(counts)
	}
}

// countByStage scans the table and counts items whose sk begins with "stage#<tier>#".
// This matches the table schema where SK = stage#bronze#<uuid>, stage#silver#<uuid>, etc.
func countByStage(ctx context.Context, ddb *dynamodb.Client, table, stage string) int {
	prefix := "stage#" + stage + "#"

	var total int32
	var lastEvaluatedKey map[string]types.AttributeValue

	for {
		out, err := ddb.Scan(ctx, &dynamodb.ScanInput{
			TableName:        aws.String(table),
			FilterExpression: aws.String("begins_with(sk, :prefix)"),
			ExpressionAttributeValues: map[string]types.AttributeValue{
				":prefix": &types.AttributeValueMemberS{Value: prefix},
			},
			Select:           types.SelectCount,
			ExclusiveStartKey: lastEvaluatedKey,
		})
		if err != nil {
			log.Printf("countByStage %s: %v", stage, err)
			return 0
		}

		total += out.Count

		if out.LastEvaluatedKey == nil || len(out.LastEvaluatedKey) == 0 {
			break
		}

		lastEvaluatedKey = out.LastEvaluatedKey
	}

	return int(total)
}

func handleServices(w http.ResponseWriter, r *http.Request) {
	raw := os.Getenv("SERVICE_URLS")
	if raw == "" {
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode([]ServiceStatus{})
		return
	}

	entries := strings.Split(raw, ",")
	results := make([]ServiceStatus, len(entries))

	var wg sync.WaitGroup
	client := &http.Client{Timeout: 5 * time.Second}

	for i, entry := range entries {
		wg.Add(1)
		go func(idx int, raw string) {
			defer wg.Done()
			parts := strings.SplitN(strings.TrimSpace(raw), "|", 2)
			name := parts[0]
			url := ""
			if len(parts) == 2 {
				url = parts[1]
			} else {
				url = parts[0]
			}

			start := time.Now()
			resp, err := client.Get(url + "/health")
			elapsed := time.Since(start).Milliseconds()

			status := "ok"
			if err != nil {
				status = "error"
			} else {
				resp.Body.Close()
				if resp.StatusCode >= 400 {
					status = "error"
				}
			}
			results[idx] = ServiceStatus{
				Name:      name,
				URL:       url,
				Status:    status,
				LatencyMs: elapsed,
			}
		}(i, entry)
	}
	wg.Wait()

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(results)
}

// ---------------------------------------------------------------------------
// CORS helper
// ---------------------------------------------------------------------------

func withCORS(next http.HandlerFunc) http.HandlerFunc {
	allowedRaw := os.Getenv("ALLOWED_ORIGINS")

	// Precompute the list of allowed origins from ALLOWED_ORIGINS.
	// Supports:
	//   - "*" (allow all origins)
	//   - single origin (e.g. "https://example.com")
	//   - comma-separated list of origins (e.g. "https://a.com,https://b.com")
	var allowedOrigins []string
	if allowedRaw != "" && allowedRaw != "*" {
		for _, part := range strings.Split(allowedRaw, ",") {
			if origin := strings.TrimSpace(part); origin != "" {
				allowedOrigins = append(allowedOrigins, origin)
			}
		}
	}

	return func(w http.ResponseWriter, r *http.Request) {
		origin := r.Header.Get("Origin")

		if allowedRaw == "*" {
			// Explicit wildcard: allow any origin.
			w.Header().Set("Access-Control-Allow-Origin", "*")
		} else if origin != "" && len(allowedOrigins) > 0 {
			// Echo back the Origin header if it is in the allowlist.
			for _, allowed := range allowedOrigins {
				if origin == allowed {
					w.Header().Set("Access-Control-Allow-Origin", origin)
					break
				}
			}
		}

		w.Header().Set("Access-Control-Allow-Headers", "Content-Type, X-Admin-Key, Authorization")
		w.Header().Set("Access-Control-Allow-Methods", "GET, OPTIONS")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		next(w, r)
	}
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

func main() {
	// AWS DynamoDB client
	cfg, err := config.LoadDefaultConfig(context.Background())
	if err != nil {
		log.Fatalf("unable to load AWS config: %v", err)
	}
	ddb := dynamodb.NewFromConfig(cfg)

	mux := http.NewServeMux()
	mux.HandleFunc("/health", withCORS(handleHealth))
	mux.HandleFunc("/api/pipeline", withCORS(requireAdmin(handlePipeline(ddb))))
	mux.HandleFunc("/api/services", withCORS(requireAdmin(handleServices)))

	port := os.Getenv("PORT")
	if port == "" {
		port = "8090"
	}
	addr := fmt.Sprintf(":%s", port)
	log.Printf("go-pipeline-monitor listening on %s", addr)
	if err := http.ListenAndServe(addr, mux); err != nil {
		log.Fatal(err)
	}
}
