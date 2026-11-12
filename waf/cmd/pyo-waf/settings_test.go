package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

// The ConfigMap's changes are taken as the API sends them: the list's
// copy, then each change on the watch.
func TestFollowsTheConfigMap(t *testing.T) {
	cfg := func(host string) string {
		return fmt.Sprintf(`{"sets":{"a":{"engine":"On","level":1}},"hosts":{%q:"a"}}`, host)
	}
	changed := make(chan struct{})
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if req.URL.Path != "/api/v1/namespaces/pyo-waf/configmaps" || req.URL.Query().Get("fieldSelector") != "metadata.name=pyo-waf" || req.Header.Get("Authorization") != "Bearer tok" {
			http.Error(w, "forbidden", http.StatusForbidden)
			return
		}
		if req.URL.Query().Get("watch") == "" {
			json.NewEncoder(w).Encode(map[string]any{
				"metadata": map[string]any{"resourceVersion": "10"},
				"items":    []any{map[string]any{"metadata": map[string]any{"resourceVersion": "9"}, "data": map[string]string{"config.json": cfg("one.test")}}},
			})
			return
		}
		if req.URL.Query().Get("resourceVersion") != "10" {
			<-req.Context().Done()
			return
		}
		<-changed
		json.NewEncoder(w).Encode(map[string]any{"type": "MODIFIED", "object": map[string]any{"metadata": map[string]any{"resourceVersion": "11"}, "data": map[string]string{"config.json": cfg("two.test")}}})
		w.(http.Flusher).Flush()
		<-req.Context().Done()
	}))
	defer srv.Close()

	s := &server{}
	s.cur.Store(empty())
	r := &reloader{s: s}
	k := &kubeAPI{base: srv.URL, namespace: "pyo-waf", name: "pyo-waf", token: func() string { return "tok" }, client: srv.Client()}
	go k.follow(r)
	wait := func(host string) {
		t.Helper()
		for deadline := time.Now().Add(5 * time.Second); time.Now().Before(deadline); time.Sleep(10 * time.Millisecond) {
			if s.cur.Load().hosts[host] != nil {
				return
			}
		}
		t.Fatalf("%s never loaded", host)
	}
	wait("one.test")
	if !r.fromAPI.Load() {
		t.Fatal("the file would still be read")
	}
	close(changed)
	wait("two.test")
	if s.cur.Load().hosts["one.test"] != nil {
		t.Fatal("the old settings stayed")
	}
}

// An API that refuses: the file stays the source.
func TestRefusedAPIKeepsTheFile(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		http.Error(w, "forbidden", http.StatusForbidden)
	}))
	defer srv.Close()
	r := &reloader{s: &server{}}
	k := &kubeAPI{base: srv.URL, namespace: "pyo-waf", name: "pyo-waf", token: func() string { return "" }, client: srv.Client()}
	if _, err := k.list(t.Context(), r); err == nil {
		t.Fatal("refused, yet listed")
	}
	if r.fromAPI.Load() {
		t.Fatal("the file would be ignored")
	}
}

// The same content twice compiles once.
func TestApplyOncePerContent(t *testing.T) {
	s := &server{}
	r := &reloader{s: s}
	raw := []byte(`{"sets":{"a":{"engine":"On","level":1}},"hosts":{"x.test":"a"}}`)
	if !r.apply(raw, "file") {
		t.Fatal("not loaded")
	}
	first := s.cur.Load()
	if !r.apply(raw, "api") || s.cur.Load() != first {
		t.Fatal("reloaded the same settings")
	}
	if r.apply([]byte(`{`), "api") || s.cur.Load() != first {
		t.Fatal("broken settings replaced good ones")
	}
}
