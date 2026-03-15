FROM golang:1.22-bookworm AS builder

WORKDIR /app
COPY go.mod ./
RUN go mod download

COPY . .
RUN CGO_ENABLED=0 GOOS=linux go build -o /go-pipeline-monitor .

# ---- runtime ----
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
RUN useradd --no-create-home --shell /bin/false appuser
USER appuser

COPY --from=builder /go-pipeline-monitor /usr/local/bin/go-pipeline-monitor
EXPOSE 8090
CMD ["/usr/local/bin/go-pipeline-monitor"]
