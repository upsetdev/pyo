package main

// Where the settings come from. Pyo writes them into a ConfigMap, mounted
// as a file; but the kubelet refreshes a mounted ConfigMap only on its own
// sync, a minute or more after a change, and until then a service whose WAF
// was just turned on passes uninspected. So the processor also follows the
// ConfigMap through the Kubernetes API (its role may read only that one),
// and takes a change within a second. The file is the fallback while the
// API can't be read.

import (
	"context"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/corazawaf/coraza/v3"
)

// reloader compiles settings into the server, once per distinct content.
type reloader struct {
	s       *server
	mu      sync.Mutex
	cache   map[string]coraza.WAF
	lastSum [32]byte
	loaded  bool
	// The API delivered the settings and is being followed: the file,
	// refreshed later and maybe with an older copy, is left alone.
	fromAPI atomic.Bool
}

// apply loads raw unless it's what's loaded already. Whether the settings
// in effect are raw's.
func (r *reloader) apply(raw []byte, source string) bool {
	sum := sha256.Sum256(raw)
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.loaded && sum == r.lastSum {
		return true
	}
	next, cache, err := compile(raw, r.cache)
	if err != nil {
		slog.Error("settings not loaded; keeping the previous ones", "source", source, "error", err)
		return false
	}
	r.s.cur.Store(next)
	r.cache, r.lastSum, r.loaded = cache, sum, true
	slog.Info("settings loaded", "source", source, "hosts", len(next.hosts), "hostsWithRules", len(next.rules))
	return true
}

// pollFile re-reads the file when it changes, while the API isn't followed.
func (r *reloader) pollFile(path string) {
	var last time.Time
	if st, err := os.Stat(path); err == nil {
		last = st.ModTime()
	}
	for range time.Tick(5 * time.Second) {
		st, err := os.Stat(path)
		if err != nil || !st.ModTime().After(last) || r.fromAPI.Load() {
			continue
		}
		last = st.ModTime()
		if raw, err := os.ReadFile(path); err == nil {
			r.apply(raw, "file")
		}
	}
}

// kubeAPI reads one ConfigMap.
type kubeAPI struct {
	base      string // https://host:port
	namespace string
	name      string
	token     func() string
	client    *http.Client
}

const saDir = "/var/run/secrets/kubernetes.io/serviceaccount"

// inCluster: the API as the pod's service account sees it.
func inCluster(name string) (*kubeAPI, error) {
	host, port := os.Getenv("KUBERNETES_SERVICE_HOST"), os.Getenv("KUBERNETES_SERVICE_PORT")
	if host == "" || port == "" {
		return nil, errors.New("not in a cluster")
	}
	ns, err := os.ReadFile(saDir + "/namespace")
	if err != nil {
		return nil, fmt.Errorf("no service account: %w", err)
	}
	ca, err := os.ReadFile(saDir + "/ca.crt")
	if err != nil {
		return nil, err
	}
	pool := x509.NewCertPool()
	if !pool.AppendCertsFromPEM(ca) {
		return nil, errors.New("no CA certificate")
	}
	return &kubeAPI{
		base:      "https://" + net.JoinHostPort(host, port),
		namespace: strings.TrimSpace(string(ns)),
		name:      name,
		// Re-read each time: the kubelet rotates it.
		token: func() string {
			t, _ := os.ReadFile(saDir + "/token")
			return strings.TrimSpace(string(t))
		},
		client: &http.Client{Transport: &http.Transport{
			TLSClientConfig:     &tls.Config{RootCAs: pool, MinVersion: tls.VersionTLS12},
			TLSHandshakeTimeout: 10 * time.Second,
		}},
	}, nil
}

type configMap struct {
	Metadata struct {
		ResourceVersion string `json:"resourceVersion"`
	} `json:"metadata"`
	Data map[string]string `json:"data"`
}

// get makes one request for the ConfigMap (a list, or a watch with `rv`).
func (k *kubeAPI) get(ctx context.Context, watch bool, rv string) (*http.Response, error) {
	q := url.Values{"fieldSelector": {"metadata.name=" + k.name}}
	if watch {
		q.Set("watch", "1")
		q.Set("resourceVersion", rv)
		q.Set("allowWatchBookmarks", "true")
		q.Set("timeoutSeconds", "300")
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, fmt.Sprintf("%s/api/v1/namespaces/%s/configmaps?%s", k.base, url.PathEscape(k.namespace), q.Encode()), nil)
	if err != nil {
		return nil, err
	}
	if t := k.token(); t != "" {
		req.Header.Set("Authorization", "Bearer "+t)
	}
	req.Header.Set("Accept", "application/json")
	resp, err := k.client.Do(req)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 512))
		resp.Body.Close()
		return nil, fmt.Errorf("%s: %s", resp.Status, strings.TrimSpace(string(b)))
	}
	return resp, nil
}

// list loads the ConfigMap as it is now. Its resource version, to watch from.
func (k *kubeAPI) list(ctx context.Context, r *reloader) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	resp, err := k.get(ctx, false, "")
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	var l struct {
		Metadata struct {
			ResourceVersion string `json:"resourceVersion"`
		} `json:"metadata"`
		Items []configMap `json:"items"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&l); err != nil {
		return "", err
	}
	if len(l.Items) == 1 {
		if raw, ok := l.Items[0].Data["config.json"]; ok && !r.apply([]byte(raw), "api") {
			return "", errors.New("settings didn't load")
		}
	}
	return l.Metadata.ResourceVersion, nil
}

// watch loads each change until the watch ends. The resource version it
// got to; errGone when it must list again.
func (k *kubeAPI) watch(ctx context.Context, r *reloader, rv string) (string, error) {
	// Past the server's own timeout: a watch that went quiet is restarted.
	ctx, cancel := context.WithTimeout(ctx, 330*time.Second)
	defer cancel()
	resp, err := k.get(ctx, true, rv)
	if err != nil {
		return rv, err
	}
	defer resp.Body.Close()
	dec := json.NewDecoder(resp.Body)
	for {
		var ev struct {
			Type   string          `json:"type"`
			Object json.RawMessage `json:"object"`
		}
		if err := dec.Decode(&ev); err != nil {
			if errors.Is(err, io.EOF) {
				return rv, nil
			}
			return rv, err
		}
		if ev.Type == "ERROR" {
			// Mostly 410 Gone: the version is too old to watch from.
			return rv, errGone
		}
		var cm configMap
		if err := json.Unmarshal(ev.Object, &cm); err != nil {
			return rv, err
		}
		if cm.Metadata.ResourceVersion != "" {
			rv = cm.Metadata.ResourceVersion
		}
		if ev.Type == "ADDED" || ev.Type == "MODIFIED" {
			if raw, ok := cm.Data["config.json"]; ok {
				r.apply([]byte(raw), "api")
			}
		}
	}
}

var errGone = errors.New("watch expired")

// follow keeps the settings in step with the ConfigMap, for good.
func (k *kubeAPI) follow(r *reloader) {
	ctx := context.Background()
	backoff := time.Second
	warned := false
	for {
		rv, err := k.list(ctx, r)
		if err != nil {
			r.fromAPI.Store(false)
			if !warned {
				slog.Warn("can't read the settings from the API; using the file", "error", err)
				warned = true
			}
			time.Sleep(backoff)
			backoff = min(backoff*2, 30*time.Second)
			continue
		}
		if warned {
			slog.Info("reading the settings from the API again")
			warned = false
		}
		r.fromAPI.Store(true)
		backoff = time.Second
		for {
			rv, err = k.watch(ctx, r, rv)
			if err != nil {
				if !errors.Is(err, errGone) {
					slog.Warn("settings watch ended", "error", err)
					time.Sleep(time.Second)
				}
				break
			}
		}
	}
}
