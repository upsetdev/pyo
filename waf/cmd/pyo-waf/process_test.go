package main

import (
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	corev3 "github.com/envoyproxy/go-control-plane/envoy/config/core/v3"
	extprocfilter "github.com/envoyproxy/go-control-plane/envoy/extensions/filters/http/ext_proc/v3"
	extproc "github.com/envoyproxy/go-control-plane/envoy/service/ext_proc/v3"
	"google.golang.org/grpc"
)

// fakeStream plays the proxy's side of one ext_proc stream: it hands the
// processor one message at a time and records each answer.
type fakeStream struct {
	grpc.ServerStream
	in  chan *extproc.ProcessingRequest
	out chan *extproc.ProcessingResponse
}

func (f *fakeStream) Recv() (*extproc.ProcessingRequest, error) {
	r, ok := <-f.in
	if !ok {
		return nil, io.EOF
	}
	return r, nil
}

func (f *fakeStream) Send(r *extproc.ProcessingResponse) error {
	f.out <- r
	return nil
}

// stream starts Process on a fake stream; send hands it a message and
// returns its answer.
func stream(t *testing.T, s *server) (send func(*extproc.ProcessingRequest) *extproc.ProcessingResponse) {
	t.Helper()
	f := &fakeStream{in: make(chan *extproc.ProcessingRequest), out: make(chan *extproc.ProcessingResponse)}
	done := make(chan struct{})
	go func() { s.Process(f); close(done) }()
	t.Cleanup(func() { close(f.in); <-done })
	return func(r *extproc.ProcessingRequest) *extproc.ProcessingResponse {
		f.in <- r
		return <-f.out
	}
}

func testServer(t *testing.T) *server {
	t.Helper()
	p := filepath.Join(t.TempDir(), "config.json")
	os.WriteFile(p, []byte(`{
		"sets": {"b": {"engine": "On", "level": 1}, "l": {"engine": "DetectionOnly", "level": 1}},
		"hosts": {"block.test": "b", "log.test": "l"},
		"lists": {"r": [
			{"id": 1, "name": "evil header", "match": "all", "action": "block", "conditions": [{"field": "header", "name": "x-evil", "op": "equals", "values": ["1"]}]},
			{"id": 2, "name": "evil body", "match": "all", "action": "block", "conditions": [{"field": "body", "op": "contains", "values": ["EVILBODY"]}]}
		]},
		"hostRules": {"block.test": "r", "log.test": "r"}}`), 0o600)
	c, _, err := load(p, nil)
	if err != nil {
		t.Fatal(err)
	}
	s := &server{}
	s.cur.Store(c)
	return s
}

func headers(mode extprocfilter.ProcessingMode_BodySendMode, host, method, path string, eos bool, extra ...string) *extproc.ProcessingRequest {
	hs := []*corev3.HeaderValue{{Key: ":authority", RawValue: []byte(host)}, {Key: ":method", RawValue: []byte(method)}, {Key: ":path", RawValue: []byte(path)}}
	for i := 0; i+1 < len(extra); i += 2 {
		hs = append(hs, &corev3.HeaderValue{Key: extra[i], RawValue: []byte(extra[i+1])})
	}
	return &extproc.ProcessingRequest{
		ProtocolConfig: &extproc.ProtocolConfiguration{RequestBodyMode: mode},
		Request:        &extproc.ProcessingRequest_RequestHeaders{RequestHeaders: &extproc.HttpHeaders{Headers: &corev3.HeaderMap{Headers: hs}, EndOfStream: eos}},
	}
}

func body(b string, eos bool) *extproc.ProcessingRequest {
	return &extproc.ProcessingRequest{Request: &extproc.ProcessingRequest_RequestBody{RequestBody: &extproc.HttpBody{Body: []byte(b), EndOfStream: eos}}}
}

func denied(r *extproc.ProcessingResponse) bool {
	return r.GetImmediateResponse() != nil && r.GetImmediateResponse().GetStatus().GetCode() == 403
}

const (
	streamed = extprocfilter.ProcessingMode_STREAMED
	partial  = extprocfilter.ProcessingMode_BUFFERED_PARTIAL
	sqli     = "/search?q=1%27%20UNION%20SELECT%20password%20FROM%20users--"
	form     = "application/x-www-form-urlencoded"
)

// What doesn't need the body is decided at the headers, in either body
// mode: a request with a body to come never gets "go on" first.
func TestBlockedAtHeaders(t *testing.T) {
	s := testServer(t)
	for _, mode := range []extprocfilter.ProcessingMode_BodySendMode{streamed, partial} {
		for name, h := range map[string]*extproc.ProcessingRequest{
			"sqli in the query":       headers(mode, "block.test", "GET", sqli, false, "content-length", "1"),
			"xss in the query":        headers(mode, "block.test", "POST", "/?q=%3Cscript%3Ealert(1)%3C%2Fscript%3E", false, "content-type", form),
			"custom header":           headers(mode, "block.test", "POST", "/ok", false, "x-evil", "1", "content-type", form),
			"custom header, log mode": headers(mode, "log.test", "POST", "/ok", false, "x-evil", "1", "content-type", form),
		} {
			if r := stream(t, s)(h); !denied(r) {
				t.Errorf("%v %s: %v, want 403 at the headers", mode, name, r)
			}
		}
	}
}

// Log mode's OWASP rules only log: nothing is decided before the body.
func TestLogModeOwaspWaitsForTheBody(t *testing.T) {
	send := stream(t, testServer(t))
	if r := send(headers(streamed, "log.test", "GET", sqli, false, "content-length", "1")); r.GetRequestHeaders() == nil {
		t.Fatalf("%v", r)
	}
	if r := send(body("x", true)); r.GetRequestBody() == nil {
		t.Fatalf("%v", r)
	}
}

// Block mode (BUFFERED_PARTIAL): the proxy holds the request until the
// processor answers the one body message, which is decided on even when
// the body goes on past the proxy's buffer.
func TestBufferedPartialDecidesOnTheOneBodyMessage(t *testing.T) {
	s := testServer(t)
	send := stream(t, s)
	if r := send(headers(partial, "block.test", "POST", "/upload", false, "content-type", form, "content-length", "1048576")); r.GetRequestHeaders() == nil {
		t.Fatalf("headers: %v", r)
	}
	if r := send(body("hello EVILBODY", false)); !denied(r) {
		t.Fatalf("a malicious first part: %v", r)
	}

	send = stream(t, s)
	send(headers(partial, "block.test", "POST", "/login", false, "content-type", form, "content-length", "100"))
	if r := send(body("user=admin%27%20OR%20%271%27%3D%271--", false)); !denied(r) {
		t.Fatalf("sqli in a form: %v", r)
	}

	send = stream(t, s)
	send(headers(partial, "block.test", "POST", "/login", false, "content-type", form, "content-length", "100"))
	if r := send(body("user=alice&password=secret", false)); r.GetRequestBody() == nil {
		t.Fatalf("a clean first part: %v", r)
	}
}

// A clean request with a multipart body isn't refused at its headers (the
// early OWASP pass sees no body, so no body parsing error).
func TestMultipartHeadersPass(t *testing.T) {
	send := stream(t, testServer(t))
	r := send(headers(partial, "block.test", "POST", "/upload", false, "content-type", "multipart/form-data; boundary=XyZ", "content-length", "4194443"))
	if r.GetRequestHeaders() == nil {
		t.Fatalf("%v", r)
	}
	r = send(body("--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nhello\r\n--XyZ--\r\n", true))
	if r.GetRequestBody() == nil {
		t.Fatalf("%v", r)
	}
}

// Streamed (log mode): pieces pass as they're answered; the end of the
// body is held back by a verdict on it.
func TestStreamedDecidesAtTheEnd(t *testing.T) {
	send := stream(t, testServer(t))
	send(headers(streamed, "log.test", "POST", "/upload", false, "content-type", form, "content-length", "11"))
	if r := send(body("EVILBODY", false)); r.GetRequestBody() == nil {
		t.Fatalf("%v", r)
	}
	if r := send(body("abc", true)); !denied(r) {
		t.Fatalf("%v", r)
	}
}

// A block-mode body over the proxy's buffer comes cut short: its first
// part is inspected, and being cut isn't a parsing error.
func TestPartialBodies(t *testing.T) {
	s := testServer(t)
	big := strings.Repeat("a", inspectBody+50000)
	for _, tc := range []struct {
		name, ctype, body string
		block             bool
	}{
		{"multipart cut mid-file", "multipart/form-data; boundary=XyZ", "--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n" + big, false},
		{"JSON cut short", "application/json", `{"items":["` + big, false},
		{"form, attack up front", form, "user=admin%27%20OR%20%271%27%3D%271--&pad=" + big, true},
		{"form, attack past the inspected part", form, "pad=" + big + "&user=admin%27%20OR%20%271%27%3D%271--", false},
		{"multipart, attack in a field up front", "multipart/form-data; boundary=XyZ", "--XyZ\r\nContent-Disposition: form-data; name=\"q\"\r\n\r\n<script>alert(1)</script>\r\n--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n" + big, true},
	} {
		send := stream(t, s)
		if r := send(headers(partial, "block.test", "POST", "/upload", false, "content-type", tc.ctype, "content-length", "4000000")); r.GetRequestHeaders() == nil {
			t.Fatalf("%s: headers: %v", tc.name, r)
		}
		r := send(body(tc.body, false))
		if denied(r) != tc.block || (!tc.block && r.GetRequestBody() == nil) {
			t.Errorf("%s: %v, want blocked=%v", tc.name, r, tc.block)
		}
	}
}

// The mark that a body was cut short comes from the WAF only: a client's
// doesn't excuse a body that doesn't parse.
func TestPartialMarkFromClientsIgnored(t *testing.T) {
	send := stream(t, testServer(t))
	send(headers(partial, "block.test", "POST", "/api", false, "content-type", "application/json", "content-length", "12", partialMark, "1"))
	if r := send(body(`{"a": [1, 2`, true)); r.GetImmediateResponse() == nil || r.GetImmediateResponse().GetStatus().GetCode() != 400 {
		t.Fatalf("a malformed body: %v, want 400", r)
	}
}

// A body is inspected up to inspectBody, so one huge field is decided
// quickly, well within block mode's 5 s.
func TestBodiesWithoutFilesAreCapped(t *testing.T) {
	s := testServer(t)
	huge := strings.Repeat("a", 2<<20)
	for _, tc := range []struct {
		name, ctype, body string
		block             bool
	}{
		{"form, attack up front", form, "user=admin%27%20OR%20%271%27%3D%271--&pad=" + huge, true},
		{"form, one huge field", form, "pad=" + huge, false},
		{"form, attack past the cap", form, "pad=" + strings.Repeat("a", inspectBody) + "&user=admin%27%20OR%20%271%27%3D%271--", false},
		{"JSON, one huge value", "application/json", `{"a":"` + huge + `"}`, false},
		{"multipart, one huge text field", "multipart/form-data; boundary=XyZ", "--XyZ\r\nContent-Disposition: form-data; name=\"q\"\r\n\r\n" + huge + "\r\n--XyZ--\r\n", false},
	} {
		for _, mode := range []extprocfilter.ProcessingMode_BodySendMode{partial, streamed} {
			send := stream(t, s)
			send(headers(mode, "block.test", "POST", "/upload", false, "content-type", tc.ctype, "content-length", "3000000"))
			t0 := time.Now()
			r := send(body(tc.body, mode == streamed))
			if took := time.Since(t0); took > 2*time.Second {
				t.Errorf("%s (%v): took %v", tc.name, mode, took)
			}
			if denied(r) != tc.block || (!tc.block && r.GetRequestBody() == nil) {
				t.Errorf("%s (%v): %v, want blocked=%v", tc.name, mode, r, tc.block)
			}
		}
	}
}

// Log only: the OWASP rules can't stop a request, so its body isn't held
// for them; custom rules on the body still decide at once.
func TestLogOnlyDoesntWaitForOwasp(t *testing.T) {
	s := testServer(t)
	send := stream(t, s)
	send(headers(streamed, "log.test", "POST", "/login", false, "content-type", "multipart/form-data; boundary=XyZ", "content-length", "3000000"))
	// A big text field takes the OWASP rules about a second.
	big := "--XyZ\r\nContent-Disposition: form-data; name=\"q\"\r\n\r\n" + strings.Repeat("a", 1<<20) + "\r\n--XyZ--\r\n"
	t0 := time.Now()
	if r := send(body(big, true)); r.GetRequestBody() == nil {
		t.Fatalf("%v", r)
	}
	if took := time.Since(t0); took > 200*time.Millisecond {
		t.Errorf("held %v", took)
	}
	send = stream(t, s)
	send(headers(streamed, "log.test", "POST", "/x", false, "content-type", form, "content-length", "100"))
	if r := send(body("a=EVILBODY", true)); !denied(r) {
		t.Fatalf("custom body rule: %v", r)
	}
}

// A path is left out of the OWASP rules with a custom rule: "Skip OWASP"
// (later custom rules still run) or "Allow" (nothing after it runs), on a
// path that starts with it. Other paths are still inspected.
func TestCustomRuleSkipsAPath(t *testing.T) {
	p := filepath.Join(t.TempDir(), "config.json")
	os.WriteFile(p, []byte(`{
		"sets": {"b": {"engine": "On", "level": 1}},
		"hosts": {"skip.test": "b", "allow.test": "b"},
		"lists": {
			"s": [
				{"id": 1, "name": "skip webhooks", "match": "all", "action": "skipOwasp", "conditions": [{"field": "path", "op": "startsWith", "values": ["/api/webhooks/"]}]},
				{"id": 2, "name": "evil header", "match": "all", "action": "block", "conditions": [{"field": "header", "name": "x-evil", "op": "equals", "values": ["1"]}]}
			],
			"a": [
				{"id": 3, "name": "allow webhooks", "match": "all", "action": "allow", "conditions": [{"field": "path", "op": "startsWith", "values": ["/api/webhooks/"]}]},
				{"id": 4, "name": "evil header", "match": "all", "action": "block", "conditions": [{"field": "header", "name": "x-evil", "op": "equals", "values": ["1"]}]}
			]
		},
		"hostRules": {"skip.test": "s", "allow.test": "a"}}`), 0o600)
	c, _, err := load(p, nil)
	if err != nil {
		t.Fatal(err)
	}
	s := &server{}
	s.cur.Store(c)
	const hook = "/api/webhooks/stripe?q=1%27%20UNION%20SELECT%20password%20FROM%20users--"
	const attack = "q=1' UNION SELECT password FROM users--"
	for _, host := range []string{"skip.test", "allow.test"} {
		// Without a body, and with one (decided at its end).
		if r := stream(t, s)(headers(streamed, host, "GET", hook, true)); denied(r) {
			t.Errorf("%s: a skipped path was inspected", host)
		}
		send := stream(t, s)
		if r := send(headers(partial, host, "POST", "/api/webhooks/stripe", false, "content-type", form)); denied(r) {
			t.Fatalf("%s: refused at the headers", host)
		}
		if r := send(body(attack, true)); denied(r) {
			t.Errorf("%s: a skipped path's body was inspected", host)
		}
		// Anywhere else, the OWASP rules still run.
		if r := stream(t, s)(headers(streamed, host, "GET", sqli, true)); !denied(r) {
			t.Errorf("%s: another path wasn't inspected", host)
		}
		if r := stream(t, s)(headers(streamed, host, "GET", "/api/webhook?q=1%27%20UNION%20SELECT%20password%20FROM%20users--", true)); !denied(r) {
			t.Errorf("%s: only paths that start with it are skipped", host)
		}
	}
	// Skip OWASP leaves later custom rules to run; Allow doesn't.
	if r := stream(t, s)(headers(streamed, "skip.test", "GET", "/api/webhooks/x", true, "x-evil", "1")); !denied(r) {
		t.Error("skipOwasp: a later custom rule didn't run")
	}
	if r := stream(t, s)(headers(streamed, "allow.test", "GET", "/api/webhooks/x", true, "x-evil", "1")); denied(r) {
		t.Error("allow: a later custom rule ran")
	}
}

// The handshake of a WebSocket.
var ws = []string{"connection", "Upgrade", "upgrade", "websocket", "sec-websocket-version", "13", "sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="}

func upgrade(mode extprocfilter.ProcessingMode_BodySendMode, host, path string, extra ...string) *extproc.ProcessingRequest {
	return headers(mode, host, "GET", path, false, append(append([]string{}, ws...), extra...)...)
}

// overridden: the answer goes on and tells the proxy to send no body (and
// nothing of the response), as the route has it otherwise.
func overridden(r *extproc.ProcessingResponse) bool {
	m := r.GetRequestHeaders() != nil && r.GetModeOverride() != nil
	if !m {
		return false
	}
	o := r.GetModeOverride()
	return o.GetRequestBodyMode() == extprocfilter.ProcessingMode_NONE &&
		o.GetResponseBodyMode() == extprocfilter.ProcessingMode_NONE &&
		o.GetResponseHeaderMode() == extprocfilter.ProcessingMode_SKIP &&
		o.GetRequestTrailerMode() == extprocfilter.ProcessingMode_SKIP &&
		o.GetResponseTrailerMode() == extprocfilter.ProcessingMode_SKIP
}

// An upgrade sends nothing before its 101: it's decided at its headers,
// in every mode, and the proxy is told not to wait for a body.
func TestUpgradeGoesOnWithoutABody(t *testing.T) {
	s := testServer(t)
	for _, mode := range []extprocfilter.ProcessingMode_BodySendMode{partial, streamed} {
		for _, host := range []string{"block.test", "log.test", "other.test"} {
			if r := stream(t, s)(upgrade(mode, host, "/chat")); !overridden(r) {
				t.Errorf("%v %s: %v, want go on with no body", mode, host, r)
			}
		}
		// Log only: an attack is logged, not refused.
		if r := stream(t, s)(upgrade(mode, "log.test", "/chat?q=%3Cscript%3Ealert(1)%3C%2Fscript%3E")); !overridden(r) {
			t.Errorf("%v log only: %v", mode, r)
		}
	}
}

// The handshake gets every rule a request without a body gets: OWASP rules
// on the request line and headers, and custom rules.
func TestUpgradeIsInspected(t *testing.T) {
	s := testServer(t)
	for _, mode := range []extprocfilter.ProcessingMode_BodySendMode{partial, streamed} {
		for name, h := range map[string]*extproc.ProcessingRequest{
			"xss in the query":    upgrade(mode, "block.test", "/chat?q=%3Cscript%3Ealert(1)%3C%2Fscript%3E"),
			"sqli in the query":   upgrade(mode, "block.test", sqli),
			"xss in a header":     upgrade(mode, "block.test", "/chat", "referer", "http://example.test/<script>alert(1)</script>"),
			"scanner user agent":  upgrade(mode, "block.test", "/chat", "user-agent", "sqlmap/1.7"),
			"path traversal":      upgrade(mode, "block.test", "/chat/../../etc/passwd"),
			"custom header":       upgrade(mode, "block.test", "/chat", "x-evil", "1"),
			"custom header, log":  upgrade(mode, "log.test", "/chat", "x-evil", "1"),
			"body by length":      upgrade(mode, "block.test", "/chat", "content-length", "12"),
			"body by length, log": upgrade(mode, "log.test", "/chat", "content-length", "12"),
			"chunked body":        upgrade(mode, "block.test", "/chat", "transfer-encoding", "chunked"),
			"chunked body, log":   upgrade(mode, "log.test", "/chat", "transfer-encoding", "chunked"),
		} {
			if r := stream(t, s)(h); !denied(r) {
				t.Errorf("%v %s: %v, want 403", mode, name, r)
			}
		}
		if r := stream(t, s)(upgrade(mode, "block.test", "/chat", "content-length", "0")); !overridden(r) {
			t.Errorf("%v an empty body: %v", mode, r)
		}
	}
}

// A route that doesn't let the WAF change its mode (an older policy) sends
// a body all the same: it's decided on as before.
func TestUpgradeBodyWithoutOverride(t *testing.T) {
	s := testServer(t)
	send := stream(t, s)
	send(upgrade(partial, "block.test", "/chat"))
	if r := send(body("EVILBODY", false)); !denied(r) {
		t.Fatalf("%v", r)
	}
	send = stream(t, s)
	send(upgrade(partial, "block.test", "/chat", "content-type", form))
	if r := send(body("hello", false)); r.GetRequestBody() == nil {
		t.Fatalf("%v", r)
	}
}

// Anything else keeps the route's mode.
func TestOnlyUpgradesOverride(t *testing.T) {
	s := testServer(t)
	for _, h := range []*extproc.ProcessingRequest{
		headers(partial, "block.test", "POST", "/form", false, "content-type", form, "content-length", "3"),
		headers(partial, "block.test", "GET", "/", true),
		headers(partial, "block.test", "GET", "/", false, "upgrade", "websocket", "connection", "keep-alive"),
		headers(partial, "other.test", "POST", "/", false, "content-length", "3"),
	} {
		if r := stream(t, s)(h); r.GetModeOverride() != nil || r.GetRequestHeaders() == nil {
			t.Errorf("%v", r)
		}
	}
}

func TestIsUpgrade(t *testing.T) {
	for _, tc := range []struct {
		h    map[string]string
		want bool
	}{
		{map[string]string{"upgrade": "websocket", "connection": "Upgrade"}, true},
		{map[string]string{"upgrade": "websocket", "connection": "keep-alive, Upgrade"}, true},
		{map[string]string{"upgrade": "websocket", "connection": "keep-alive"}, false},
		{map[string]string{"upgrade": "websocket"}, false},
		{map[string]string{"connection": "upgrade"}, false},
		{map[string]string{}, false},
	} {
		if got := isUpgrade(tc.h); got != tc.want {
			t.Errorf("%v: %v", tc.h, got)
		}
	}
	for cl, want := range map[string]bool{"": false, "0": false, "00": false, " 0 ": false, "1": true, "12": true} {
		if got := declaresBody(map[string]string{"content-length": cl}); got != want {
			t.Errorf("content-length %q: %v", cl, got)
		}
	}
}
