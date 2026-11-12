// pyo-waf: Coraza with the OWASP Core Rule Set, and custom rules, as an
// Envoy external processor (ext_proc). The traffic proxy sends it each
// request to a service that has the WAF on (Pyo's EnvoyExtensionPolicy); it
// answers "go on" or a 403, and logs what matched as one JSON line per
// request on stdout.
//
// Settings come from a JSON file (Pyo writes it, a ConfigMap), re-read when
// it changes (settings.go):
//
//	{"sets": {"<name>": {"engine": "On" | "DetectionOnly", "level": 1-4,
//	                     "exceptions": [942100], "threshold": 0}},
//	 "hosts": {"<hostname>": "<set name>"},
//	 "lists": {"<name>": [<custom rule>, ...]},
//	 "hostRules": {"<hostname>": "<list name>"}}
//
// A host's custom rules run first (rules.go), then its set's OWASP rules,
// unless a custom rule said not to ("allow" or "skipOwasp": how a path is
// left out of them). A host with neither isn't inspected. OWASP rules are compiled once per set
// and shared by every request, so memory grows with the number of distinct
// settings, not with traffic or domains.
package main

import (
	"crypto/sha256"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os"
	"os/signal"
	"runtime"
	"strings"
	"sync/atomic"
	"syscall"
	"time"

	coreruleset "github.com/corazawaf/coraza-coreruleset/v4"
	"github.com/corazawaf/coraza/v3"
	"github.com/corazawaf/coraza/v3/types"
	corev3 "github.com/envoyproxy/go-control-plane/envoy/config/core/v3"
	extprocfilter "github.com/envoyproxy/go-control-plane/envoy/extensions/filters/http/ext_proc/v3"
	extproc "github.com/envoyproxy/go-control-plane/envoy/service/ext_proc/v3"
	typev3 "github.com/envoyproxy/go-control-plane/envoy/type/v3"
	"github.com/oschwald/maxminddb-golang"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/health"
	healthpb "google.golang.org/grpc/health/grpc_health_v1"
	"google.golang.org/grpc/status"
)

// Set is one combination of settings: a WAF compiled for it.
type Set struct {
	Engine     string `json:"engine"`
	Level      int    `json:"level"`
	Exceptions []int  `json:"exceptions"`
	// The Core Rule Set's inbound anomaly score at which a request is
	// blocked (5, its default, when unset).
	Threshold int `json:"threshold"`
}

type Config struct {
	Sets  map[string]Set    `json:"sets"`
	Hosts map[string]string `json:"hosts"`
	// Custom rules: lists, and which host runs which.
	Lists     map[string][]Rule `json:"lists"`
	HostRules map[string]string `json:"hostRules"`
}

type compiled struct {
	hosts map[string]coraza.WAF
	names map[string]string // host -> set name, for the log
	// Hosts whose OWASP rules block (engine On), not only log: those
	// worth running before the body.
	blocking map[string]bool
	rules    map[string][]rule // host -> its custom rules, in order
}

func empty() *compiled {
	return &compiled{hosts: map[string]coraza.WAF{}, names: map[string]string{}, blocking: map[string]bool{}, rules: map[string][]rule{}}
}

func directives(s Set) string {
	engine := "On"
	if s.Engine == "DetectionOnly" {
		engine = "DetectionOnly"
	}
	level := s.Level
	if level < 1 || level > 4 {
		level = 1
	}
	d := []string{
		"Include @coraza.conf-recommended",
		"SecAuditEngine Off",
		fmt.Sprintf("SecRequestBodyLimit %d", inspectBody),
		"SecRequestBodyLimitAction ProcessPartial",
		// A body longer than inspectBody (partialMark): Coraza inspects
		// its first part as a body it cut itself (a multipart body ending
		// mid-part isn't malformed), and a body that doesn't parse for
		// being cut (JSON, XML) isn't refused for it.
		fmt.Sprintf(`SecRule REQUEST_HEADERS:%s "@streq 1" "id:10,phase:1,pass,nolog,t:none,ctl:ruleRemoveById=200002"`, partialMark),
		"SecResponseBodyAccess Off",
		"Include @crs-setup.conf.example",
		"SecRuleEngine " + engine,
		fmt.Sprintf(`SecAction "id:900000,phase:1,pass,t:none,nolog,setvar:tx.blocking_paranoia_level=%d"`, level),
		"Include @owasp_crs/*.conf",
	}
	if s.Threshold > 0 {
		d = append(d, fmt.Sprintf(`SecAction "id:900110,phase:1,pass,t:none,nolog,setvar:tx.inbound_anomaly_score_threshold=%d"`, s.Threshold))
	}
	for _, id := range s.Exceptions {
		d = append(d, fmt.Sprintf("SecRuleRemoveById %d", id))
	}
	return strings.Join(d, "\n")
}

// load reads the settings file and compiles it (compile).
func load(path string, prev map[string]coraza.WAF) (*compiled, map[string]coraza.WAF, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, nil, err
	}
	return compile(raw, prev)
}

// compile compiles each set once; sets compiled before and unchanged are kept.
func compile(raw []byte, prev map[string]coraza.WAF) (*compiled, map[string]coraza.WAF, error) {
	var err error
	var c Config
	if err := json.Unmarshal(raw, &c); err != nil {
		return nil, nil, err
	}
	bySet := map[string]coraza.WAF{}
	cache := map[string]coraza.WAF{}
	for name, s := range c.Sets {
		d := directives(s)
		key := fmt.Sprintf("%x", sha256.Sum256([]byte(d)))
		w, ok := prev[key]
		if !ok {
			t := time.Now()
			w, err = coraza.NewWAF(coraza.NewWAFConfig().WithRootFS(coreruleset.FS).WithDirectives(d))
			if err != nil {
				return nil, nil, fmt.Errorf("set %s: %w", name, err)
			}
			slog.Info("rules compiled", "set", name, "took", time.Since(t).String())
		}
		cache[key] = w
		bySet[name] = w
	}
	out := empty()
	for host, set := range c.Hosts {
		if w, ok := bySet[set]; ok {
			out.hosts[strings.ToLower(host)] = w
			out.names[strings.ToLower(host)] = set
			out.blocking[strings.ToLower(host)] = c.Sets[set].Engine != "DetectionOnly"
		}
	}
	// A rule that doesn't check out is left out (and said so); the others
	// still run.
	lists := map[string][]rule{}
	for name, rs := range c.Lists {
		for _, r := range rs {
			cr, err := compileRule(r)
			if err != nil {
				slog.Error("custom rule left out", "list", name, "error", err)
				continue
			}
			lists[name] = append(lists[name], cr)
		}
	}
	for host, name := range c.HostRules {
		if l, ok := lists[name]; ok {
			out.rules[strings.ToLower(host)] = l
		}
	}
	return out, cache, nil
}

type server struct {
	extproc.UnimplementedExternalProcessorServer
	cur atomic.Pointer[compiled]
	geo *maxminddb.Reader
}

// hit is the log line: what matched in one request. Custom rules are
// listed by Pyo's number for them, with their name as the message.
type hit struct {
	WAF      string   `json:"waf"`
	Time     string   `json:"time"`
	Host     string   `json:"host"`
	Client   string   `json:"client"`
	Method   string   `json:"method"`
	URI      string   `json:"uri"`
	Blocked  bool     `json:"blocked"`
	Rules    []int    `json:"rules"`
	Messages []string `json:"messages"`
	Severity string   `json:"severity"`
	Set      string   `json:"set"`
}

func cont() *extproc.ProcessingResponse {
	return &extproc.ProcessingResponse{Response: &extproc.ProcessingResponse_RequestHeaders{RequestHeaders: &extproc.HeadersResponse{}}}
}

func bodyCont() *extproc.ProcessingResponse {
	return &extproc.ProcessingResponse{Response: &extproc.ProcessingResponse_RequestBody{RequestBody: &extproc.BodyResponse{}}}
}

func deny(status int) *extproc.ProcessingResponse {
	if status == 0 {
		status = 403
	}
	return &extproc.ProcessingResponse{Response: &extproc.ProcessingResponse_ImmediateResponse{ImmediateResponse: &extproc.ImmediateResponse{
		Status:  &typev3.HttpStatus{Code: typev3.StatusCode(status)},
		Body:    []byte("Blocked by the web application firewall.\n"),
		Headers: &extproc.HeaderMutation{SetHeaders: []*corev3.HeaderValueOption{{Header: &corev3.HeaderValue{Key: "content-type", RawValue: []byte("text/plain")}}}},
		Details: "pyo_waf",
	}}}
}

// client: the visitor's address as the proxy places it (the same one the
// firewall's rules see).
func client(req *extproc.ProcessingRequest) string {
	if a, ok := req.GetAttributes()["envoy.filters.http.ext_proc"]; ok {
		if v, ok := a.GetFields()["source.address"]; ok {
			addr := v.GetStringValue()
			if h, _, err := net.SplitHostPort(addr); err == nil {
				return h
			}
			return addr
		}
	}
	return ""
}

// How much of a body is inspected (custom and OWASP rules); the rest
// passes uninspected. The OWASP rules' time grows faster than a field's
// length (one form field: about 1 s at 128 KiB, 3.3 s at 500 KB, 6.5 s at
// 1 MB, in a form or a multipart body alike), and must stay well under
// block mode's 5 s. Less than the proxy's buffer in block mode (Pyo's
// WAF_BODY_LIMIT, 256 KiB), so a body the proxy cuts short is always cut
// here too.
const inspectBody = 131072

// What's kept of a body: one byte past inspectBody, so Coraza knows it
// cut it.
const maxBody = inspectBody + 1

// The request header that tells the OWASP rules a body was cut short. Set
// only by the WAF: one a client sends is dropped.
const partialMark = "x-pyo-waf-partial"

// The largest message the proxy may send: a buffered body (at most its
// buffer limit, 256 KiB) and headers, with room to spare.
const maxMessage = 16 << 20

// inspection is one request on its way through: its custom rules, then
// its OWASP rules, once the body (if any) is in.
type inspection struct {
	req      request
	owasp    coraza.WAF
	blocking bool // its OWASP rules block, not only log
	rules    []rule
	h        hit
}

// decide runs the request's custom rules, then (unless one said not to)
// its OWASP rules, logs what matched, and says whether to block (0: no).
//
// OWASP rules that only log (Log only mode) run in the background when
// the request has a body: their verdict can't stop it, so it doesn't wait
// for them. (Streamed, the proxy would hold the body's last piece until
// they finish, however long a large body takes.)
func (in *inspection) decide() int {
	o := evaluate(in.rules, &in.req)
	if o.block {
		in.log(o.matched, 403)
		return 403
	}
	if in.owasp == nil || o.skipOwasp {
		in.log(o.matched, 0)
		return 0
	}
	if !in.blocking && len(in.req.body) > 0 {
		inBackground(func() {
			in.runOwasp(&in.h, true)
			in.log(o.matched, 0)
		})
		return 0
	}
	status := in.runOwasp(&in.h, true)
	in.log(o.matched, status)
	return status
}

// Log-only inspections waiting or running in the background: at most a
// few at once, so they never take the CPU from requests; past that many
// waiting, a request's findings are dropped (and that said, now and then).
var (
	running     = make(chan struct{}, max(1, runtime.NumCPU()/2))
	waiting     atomic.Int64
	lastDropped atomic.Int64
)

const maxWaiting = 64

func inBackground(f func()) {
	if waiting.Add(1) > maxWaiting {
		waiting.Add(-1)
		if now := time.Now().Unix(); lastDropped.Swap(now) < now-10 {
			slog.Warn("busy: a log-only request went uninspected")
		}
		return
	}
	go func() {
		running <- struct{}{}
		defer func() { <-running; waiting.Add(-1) }()
		f()
	}()
}

// early decides what can be decided before the body (a request with one,
// at its headers): the custom rules on everything but the body, then the
// blocking OWASP rules on the request line and headers. A block is final
// (the request is refused at once, nothing reaches the service); 0 means
// the body decides. Matches that don't block are left for decide, which
// sees them again.
func (in *inspection) early() int {
	o, owasp := evaluateEarly(in.rules, &in.req)
	status := 0
	h := in.h
	if o.block {
		status = 403
	} else if owasp && in.owasp != nil && in.blocking {
		status = in.runOwasp(&h, false)
	}
	if status != 0 {
		in.h = h
		in.log(o.matched, status)
	}
	return status
}

// log writes the request's line when any rule matched.
func (in *inspection) log(custom []*rule, status int) {
	// Custom rules first, as they ran.
	h := in.h
	h.Rules, h.Messages = nil, nil
	for _, r := range custom {
		h.Rules = append(h.Rules, r.id)
		h.Messages = append(h.Messages, r.name)
	}
	h.Rules = append(h.Rules, in.h.Rules...)
	h.Messages = append(h.Messages, in.h.Messages...)
	h.Blocked = status != 0
	if len(h.Rules) > 0 || h.Blocked {
		h.WAF = "pyo"
		h.Time = time.Now().UTC().Format(time.RFC3339Nano)
		line, _ := json.Marshal(h)
		fmt.Println(string(line))
	}
}

// runOwasp runs the OWASP rules on the request (with its body, or before
// it: the request line and headers only) and adds what matched to h.
func (in *inspection) runOwasp(h *hit, withBody bool) int {
	tx := in.owasp.NewTransaction()
	defer func() {
		worst := types.RuleSeverityDebug
		for _, m := range tx.MatchedRules() {
			r := m.Rule()
			// CRS's own bookkeeping rules (anomaly scores, setup) aren't findings.
			if m.Message() == "" || r.ID() < 911000 || r.ID() >= 949000 && r.ID() < 960000 && !m.Disruptive() {
				continue
			}
			h.Rules = append(h.Rules, r.ID())
			h.Messages = append(h.Messages, m.Message())
			// The worst one: lower is more severe (0 is emergency).
			if h.Severity == "" || r.Severity() < worst {
				worst = r.Severity()
				h.Severity = worst.String()
			}
		}
		tx.ProcessLogging()
		tx.Close()
	}()
	tx.ProcessConnection(in.req.client, 0, "", 0)
	tx.ProcessURI(in.req.uri, in.req.method, "HTTP/1.1")
	for k, val := range in.req.headers {
		if !strings.HasPrefix(k, ":") && k != partialMark {
			tx.AddRequestHeader(k, val)
		}
	}
	// Past the limit: Coraza cuts it, and knows it did.
	if withBody && len(in.req.body) > inspectBody {
		tx.AddRequestHeader(partialMark, "1")
	}
	tx.AddRequestHeader("Host", in.req.host)
	if it := tx.ProcessRequestHeaders(); it != nil {
		return it.Status
	}
	if withBody && len(in.req.body) > 0 {
		if it, _, _ := tx.WriteRequestBody(in.req.body); it != nil {
			return it.Status
		}
	}
	if it, _ := tx.ProcessRequestBody(); it != nil {
		return it.Status
	}
	return 0
}

// Process handles one request. How its body comes depends on the route's
// mode (Pyo's waf_ext_proc):
//
//   - Block mode (BUFFERED_PARTIAL): the proxy holds the request, headers
//     included, and sends the body's first part (up to its buffer limit)
//     as one message, so the verdict comes before anything reaches the
//     service. That message is decided on even when the body goes on: the
//     rest then passes uninspected.
//   - Any other (STREAMED): each piece passes once answered, so only the
//     end of the body is held back by a verdict on it.
//
// Either way, what doesn't need the body (custom rules on the request line,
// headers, address and country; blocking OWASP rules on the request line
// and headers) is decided at the headers, so such a block never waits for
// the body and never lets any of the request through.
func (s *server) Process(stream extproc.ExternalProcessor_ProcessServer) error {
	var in *inspection
	buffered := false // the body comes as one message
	for {
		req, err := stream.Recv()
		if errors.Is(err, io.EOF) || status.Code(err) == codes.Canceled {
			return nil
		}
		if err != nil {
			return err
		}
		// Sent with the stream's first message.
		if pc := req.GetProtocolConfig(); pc != nil {
			switch pc.GetRequestBodyMode() {
			case extprocfilter.ProcessingMode_BUFFERED, extprocfilter.ProcessingMode_BUFFERED_PARTIAL:
				buffered = true
			}
		}
		var resp *extproc.ProcessingResponse
		switch v := req.Request.(type) {
		case *extproc.ProcessingRequest_RequestHeaders:
			hdrs := map[string]string{}
			for _, x := range v.RequestHeaders.GetHeaders().GetHeaders() {
				val := x.GetValue()
				if val == "" {
					val = string(x.GetRawValue())
				}
				hdrs[strings.ToLower(x.GetKey())] = val
			}
			host := strings.ToLower(hdrs[":authority"])
			if i := strings.LastIndexByte(host, ':'); i > 0 && !strings.HasSuffix(host, "]") {
				host = host[:i]
			}
			c := s.cur.Load()
			owasp := c.hosts[host]
			rules := c.rules[host]
			resp = cont()
			if owasp == nil && len(rules) == 0 {
				break
			}
			ip := client(req)
			in = &inspection{
				req:      request{method: hdrs[":method"], host: host, uri: hdrs[":path"], headers: hdrs, client: ip, geo: s.geo},
				owasp:    owasp,
				blocking: c.blocking[host],
				rules:    rules,
				h:        hit{Host: host, Client: ip, Method: hdrs[":method"], URI: hdrs[":path"], Set: c.names[host]},
			}
			if v.RequestHeaders.GetEndOfStream() {
				if st := in.decide(); st != 0 {
					resp = deny(st)
				}
				in = nil
			} else if st := in.early(); st != 0 {
				// Refused before its body: the proxy drops what follows.
				resp = deny(st)
				in = nil
			}
		case *extproc.ProcessingRequest_RequestBody:
			resp = bodyCont()
			if in == nil {
				break
			}
			if room := maxBody - len(in.req.body); room > 0 {
				b := v.RequestBody.GetBody()
				if len(b) > room {
					b = b[:room]
				}
				in.req.body = append(in.req.body, b...)
			}
			// Buffered: the only body message, even when the body goes on.
			if v.RequestBody.GetEndOfStream() || buffered {
				if st := in.decide(); st != 0 {
					resp = deny(st)
				}
				in = nil
			}
		case *extproc.ProcessingRequest_RequestTrailers:
			resp = &extproc.ProcessingResponse{Response: &extproc.ProcessingResponse_RequestTrailers{RequestTrailers: &extproc.TrailersResponse{}}}
			// A body that ended with trailers: decide now.
			if in != nil {
				if st := in.decide(); st != 0 {
					resp = deny(st)
				}
				in = nil
			}
		case *extproc.ProcessingRequest_ResponseHeaders:
			resp = &extproc.ProcessingResponse{Response: &extproc.ProcessingResponse_ResponseHeaders{ResponseHeaders: &extproc.HeadersResponse{}}}
		case *extproc.ProcessingRequest_ResponseBody:
			resp = &extproc.ProcessingResponse{Response: &extproc.ProcessingResponse_ResponseBody{ResponseBody: &extproc.BodyResponse{}}}
		case *extproc.ProcessingRequest_ResponseTrailers:
			resp = &extproc.ProcessingResponse{Response: &extproc.ProcessingResponse_ResponseTrailers{ResponseTrailers: &extproc.TrailersResponse{}}}
		default:
			resp = cont()
		}
		if err := stream.Send(resp); err != nil {
			return err
		}
	}
}

func main() {
	addr := flag.String("listen", ":9002", "gRPC address")
	path := flag.String("config", "/etc/pyo-waf/config.json", "settings file")
	geoPath := flag.String("geoip", "", "GeoIP country database (MaxMind format), for custom rules on countries")
	configMap := flag.String("configmap", "", "the settings' ConfigMap in the pod's namespace, followed through the Kubernetes API (key config.json)")
	flag.Parse()
	slog.SetDefault(slog.New(slog.NewJSONHandler(os.Stderr, nil)))

	s := &server{}
	if *geoPath != "" {
		if g, err := maxminddb.Open(*geoPath); err == nil {
			s.geo = g
		} else {
			slog.Warn("no GeoIP database; country conditions match nothing", "error", err)
		}
	}
	r := &reloader{s: s}
	if raw, err := os.ReadFile(*path); err != nil || !r.apply(raw, "file") {
		slog.Warn("no settings yet; nothing is inspected", "error", err)
		s.cur.Store(empty())
	}
	// Re-read the settings when they change: from the API as soon as their
	// ConfigMap does, and from the mounted file while the API can't be
	// read (the kubelet refreshes it only every minute or so).
	if *configMap != "" {
		if api, err := inCluster(*configMap); err == nil {
			go api.follow(r)
		} else {
			slog.Warn("settings only from the file", "error", err)
		}
	}
	go r.pollFile(*path)

	lis, err := net.Listen("tcp", *addr)
	if err != nil {
		slog.Error("listen", "error", err)
		os.Exit(1)
	}
	// Room for the largest body a buffered route sends in one message (the
	// proxy's buffer limit, Pyo's client policy) with its headers.
	g := grpc.NewServer(grpc.MaxRecvMsgSize(maxMessage))
	extproc.RegisterExternalProcessorServer(g, s)
	hs := health.NewServer()
	healthpb.RegisterHealthServer(g, hs)
	slog.Info("pyo-waf listening", "addr", *addr, "hosts", len(s.cur.Load().hosts))
	go stopGracefully(g, hs)
	if err := g.Serve(lis); err != nil {
		slog.Error("serve", "error", err)
		os.Exit(1)
	}
}

// On SIGTERM (an update or a server draining), stop without failing
// requests in flight: services in block mode refuse a request whose
// inspection breaks off. Report not ready and keep serving while the
// new connections move to the copy that replaces this one, then tell the
// proxies to go elsewhere (GOAWAY) and let the open streams finish.
func stopGracefully(g *grpc.Server, hs *health.Server) {
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGTERM, os.Interrupt)
	<-sig
	slog.Info("stopping")
	hs.Shutdown()
	time.Sleep(5 * time.Second)
	done := make(chan struct{})
	go func() { g.GracefulStop(); close(done) }()
	select {
	case <-done:
	case <-time.After(15 * time.Second):
		g.Stop()
	}
}
