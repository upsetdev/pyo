package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func mustRule(t *testing.T, r Rule) rule {
	t.Helper()
	c, err := compileRule(r)
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func cnd(field, op string, values ...string) Cond {
	return Cond{Field: field, Op: op, Values: values}
}

func req(method, uri string, headers map[string]string) *request {
	h := map[string]string{}
	for k, v := range headers {
		h[strings.ToLower(k)] = v
	}
	return &request{method: method, host: "shop.test", uri: uri, headers: h, client: "203.0.113.7"}
}

func TestConditions(t *testing.T) {
	r := req("post", "/WP-Admin/%6Cogin.php?a=1&b=2", map[string]string{"User-Agent": "curl/8", "Cookie": "session=abc; theme=dark", "X-Api-Key": "k1"})
	r.body = []byte(`{"x": "drop table"}`)
	for _, tc := range []struct {
		c    Cond
		want bool
	}{
		{cnd("path", "equals", "/WP-Admin/login.php"), true}, // decoded
		{cnd("path", "startsWith", "/wp-admin"), false},      // case counts
		{cnd("path", "regex", "(?i)^/wp-admin"), true},
		{cnd("query", "contains", "b=2"), true},
		{cnd("query", "startsWith", "?"), false},
		{cnd("method", "in", "GET", "POST"), true},
		{cnd("host", "endsWith", ".test"), true},
		{Cond{Field: "header", Name: "x-api-key", Op: "equals", Values: []string{"k1"}}, true},
		{Cond{Field: "header", Name: "x-missing", Op: "equals", Values: []string{""}}, true},
		{cnd("userAgent", "startsWith", "curl/"), true},
		{Cond{Field: "cookie", Name: "theme", Op: "equals", Values: []string{"dark"}}, true},
		{Cond{Field: "cookie", Name: "session", Op: "in", Values: []string{"x", "abc"}}, true},
		{cnd("body", "contains", "drop table"), true},
		{cnd("ip", "in", "203.0.113.0/24"), true},
		{cnd("ip", "in", "198.51.100.1", "2001:db8::/32"), false},
		{cnd("country", "in", "DE"), false}, // no database: unknown
		{Cond{Field: "path", Op: "contains", Values: []string{"admin"}, Negate: true}, true},
	} {
		c := mustRule(t, Rule{ID: 1, Match: "all", Action: "log", Conditions: []Cond{tc.c}})
		if got := c.matches(r); got != tc.want {
			t.Errorf("%+v: %v, want %v", tc.c, got, tc.want)
		}
	}
}

func TestAndOr(t *testing.T) {
	r := req("GET", "/admin", map[string]string{"User-Agent": "bot"})
	a, b := cnd("path", "equals", "/admin"), cnd("userAgent", "equals", "browser")
	if mustRule(t, Rule{Match: "all", Action: "block", Conditions: []Cond{a, b}}).matches(r) {
		t.Error("all: one condition fails, so the rule shouldn't match")
	}
	if !mustRule(t, Rule{Match: "any", Action: "block", Conditions: []Cond{a, b}}).matches(r) {
		t.Error("any: one condition holds, so the rule should match")
	}
	if mustRule(t, Rule{Match: "any", Action: "block", Conditions: []Cond{b}}).matches(r) {
		t.Error("any: nothing holds")
	}
}

func TestOrderAndActions(t *testing.T) {
	r := req("GET", "/x", nil)
	all := cnd("path", "startsWith", "/")
	rs := func(actions ...string) []rule {
		var out []rule
		for i, a := range actions {
			out = append(out, mustRule(t, Rule{ID: 1000000 + i, Name: a, Match: "all", Action: a, Conditions: []Cond{all}}))
		}
		return out
	}
	o := evaluate(rs("log", "allow", "block"), r)
	if o.block || !o.skipOwasp || len(o.matched) != 2 {
		t.Errorf("allow stops later rules and the OWASP rules: %+v", o)
	}
	o = evaluate(rs("skipOwasp", "block"), r)
	if !o.block || len(o.matched) != 2 {
		t.Errorf("skipOwasp lets later rules run: %+v", o)
	}
	o = evaluate(rs("block", "allow"), r)
	if !o.block || len(o.matched) != 1 {
		t.Errorf("the first that blocks wins: %+v", o)
	}
}

func TestBadRulesLeftOut(t *testing.T) {
	for _, r := range []Rule{
		{Match: "all", Action: "deny", Conditions: []Cond{cnd("path", "equals", "/")}},
		{Match: "most", Action: "block", Conditions: []Cond{cnd("path", "equals", "/")}},
		{Match: "all", Action: "block"},
		{Match: "all", Action: "block", Conditions: []Cond{cnd("path", "regex", "(")}},
		{Match: "all", Action: "block", Conditions: []Cond{cnd("path", "equals", "a", "b")}},
		{Match: "all", Action: "block", Conditions: []Cond{cnd("ip", "equals", "1.2.3.4")}},
		{Match: "all", Action: "block", Conditions: []Cond{cnd("ip", "in", "nope")}},
		{Match: "all", Action: "block", Conditions: []Cond{cnd("nope", "equals", "x")}},
		{Match: "all", Action: "block", Conditions: []Cond{cnd("path", "equals", strings.Repeat("a", 257))}},
	} {
		if _, err := compileRule(r); err == nil {
			t.Errorf("accepted %+v", r)
		}
	}
	// In a settings file: the bad rule goes, the good one stays.
	p := filepath.Join(t.TempDir(), "config.json")
	cfg := Config{
		Lists:     map[string][]Rule{"l0": {{ID: 1, Match: "all", Action: "block", Conditions: []Cond{cnd("path", "regex", "(")}}, {ID: 2, Match: "all", Action: "block", Conditions: []Cond{cnd("path", "equals", "/")}}}},
		HostRules: map[string]string{"Shop.Test": "l0"},
	}
	raw, _ := json.Marshal(cfg)
	os.WriteFile(p, raw, 0o600)
	c, _, err := load(p, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := c.rules["shop.test"]; len(got) != 1 || got[0].id != 2 {
		t.Fatalf("rules: %+v", got)
	}
}

// FuzzValues: whatever a value holds (quotes, rule-language text, odd
// bytes), it goes through the settings file as data. It compares as
// itself, never makes OWASP rules appear, and never matches anything but
// what it says.
func FuzzValues(f *testing.F) {
	for _, s := range []string{
		`/wp-admin`, `"`, `\"`, `'`, "SecRule REQUEST_URI \"@rx .*\" \"id:1,phase:1,deny\"", "a\nSecRuleEngine Off",
		`Include /etc/passwd`, `%00`, `}{"sets":{"x":{}}}`, " ", `.*`, `$(id)`,
	} {
		f.Add(s)
	}
	f.Fuzz(func(t *testing.T, v string) {
		if len(v) > maxValue {
			v = v[:maxValue]
		}
		dir := t.TempDir()
		p := filepath.Join(dir, "config.json")
		cfg := Config{
			Lists:     map[string][]Rule{"l0": {{ID: 1000000, Name: v, Match: "all", Action: "block", Conditions: []Cond{{Field: "query", Op: "equals", Values: []string{v}}}}}},
			HostRules: map[string]string{"shop.test": "l0"},
		}
		raw, err := json.Marshal(cfg)
		if err != nil {
			t.Skip()
		}
		if err := os.WriteFile(p, raw, 0o600); err != nil {
			t.Fatal(err)
		}
		c, _, err := load(p, nil)
		if err != nil {
			t.Fatalf("load: %v", err)
		}
		if len(c.hosts) != 0 || len(c.names) != 0 {
			t.Fatalf("a value made OWASP rules: %v", c.names)
		}
		// The value as JSON carries it (invalid UTF-8 becomes U+FFFD).
		var want string
		b, _ := json.Marshal(v)
		json.Unmarshal(b, &want)
		rs := c.rules["shop.test"]
		if len(want) > maxValue {
			if len(rs) != 0 {
				t.Fatalf("an over-long value kept: %+v", rs)
			}
			return
		}
		if len(rs) != 1 || rs[0].name != want || rs[0].conds[0].Values[0] != want {
			t.Fatalf("rules: %+v", rs)
		}
		match := func(q string) bool { return evaluate(rs, &request{uri: "/?" + q}).block }
		if !strings.ContainsAny(v, "#") && !match(rs[0].conds[0].Values[0]) {
			t.Fatalf("%q doesn't match itself", v)
		}
		if match(rs[0].conds[0].Values[0] + "x") {
			t.Fatalf("%q matches more than itself", v)
		}
	})
}

// TestVectors: the cases Pyo's dashboard also checks its rule test
// against (pyo's dash/src/lib/pyo/waf-vectors.json is a copy), so both agree.
func TestVectors(t *testing.T) {
	raw, err := os.ReadFile("testdata/vectors.json")
	if err != nil {
		t.Fatal(err)
	}
	var vs []struct {
		Name string `json:"name"`
		Rule struct {
			Match      string `json:"match"`
			Conditions []Cond `json:"conditions"`
		} `json:"rule"`
		Request struct {
			Method  string            `json:"method"`
			Host    string            `json:"host"`
			URI     string            `json:"uri"`
			Headers map[string]string `json:"headers"`
			Body    string            `json:"body"`
			Client  string            `json:"client"`
			Country string            `json:"country"`
		} `json:"request"`
		Matches bool `json:"matches"`
	}
	if err := json.Unmarshal(raw, &vs); err != nil {
		t.Fatal(err)
	}
	for _, v := range vs {
		r := mustRule(t, Rule{ID: 1, Match: v.Rule.Match, Action: "block", Conditions: v.Rule.Conditions})
		cc := v.Request.Country
		q := &request{method: v.Request.Method, host: v.Request.Host, uri: v.Request.URI, headers: v.Request.Headers, body: []byte(v.Request.Body), client: v.Request.Client, country: &cc}
		if q.headers == nil {
			q.headers = map[string]string{}
		}
		if got := r.matches(q); got != v.Matches {
			t.Errorf("%s: %v, want %v", v.Name, got, v.Matches)
		}
	}
}

// Before the body, only outcomes nothing later can change.
func TestEvaluateEarly(t *testing.T) {
	r := req("GET", "/admin?x=1", map[string]string{"X-Evil": "1"})
	hdr := Rule{ID: 1, Name: "hdr", Match: "all", Action: "block", Conditions: []Cond{{Field: "header", Name: "x-evil", Op: "equals", Values: []string{"1"}}}}
	bodyAllow := Rule{ID: 2, Name: "body allow", Match: "all", Action: "allow", Conditions: []Cond{cnd("body", "contains", "ok")}}
	bodySkip := Rule{ID: 3, Name: "body skip", Match: "all", Action: "skipOwasp", Conditions: []Cond{cnd("body", "contains", "ok")}}
	bodyBlock := Rule{ID: 4, Name: "body block", Match: "all", Action: "block", Conditions: []Cond{cnd("body", "contains", "x")}}
	pathOrBody := Rule{ID: 5, Name: "path or body", Match: "any", Action: "block", Conditions: []Cond{cnd("path", "equals", "/admin"), cnd("body", "contains", "x")}}
	pathAndBody := Rule{ID: 6, Name: "path and body", Match: "all", Action: "block", Conditions: []Cond{cnd("path", "equals", "/nope"), cnd("body", "contains", "x")}}
	allow := Rule{ID: 7, Name: "allow", Match: "all", Action: "allow", Conditions: []Cond{cnd("path", "equals", "/admin")}}
	for _, tc := range []struct {
		name         string
		rules        []Rule
		block, owasp bool
	}{
		{"header rule", []Rule{hdr}, true, true},
		{"after an undecided block", []Rule{bodyBlock, hdr}, true, true},
		{"after an undecided allow", []Rule{bodyAllow, hdr}, false, false},
		{"after an undecided skipOwasp", []Rule{bodySkip, hdr}, true, false},
		{"any: the path settles it", []Rule{pathOrBody}, true, true},
		{"all: the path settles it", []Rule{pathAndBody, allow}, false, false},
		{"body only", []Rule{bodyBlock}, false, true},
		{"allowed first", []Rule{allow, hdr}, false, false},
	} {
		var rs []rule
		for _, x := range tc.rules {
			rs = append(rs, mustRule(t, x))
		}
		o, owasp := evaluateEarly(rs, r)
		if o.block != tc.block || owasp != tc.owasp {
			t.Errorf("%s: block=%v owasp=%v, want %v %v", tc.name, o.block, owasp, tc.block, tc.owasp)
		}
		// Never a different verdict from the whole request's, whatever its body.
		for _, b := range []string{"", "ok", "x"} {
			full := *r
			full.body = []byte(b)
			if o.block && !evaluate(rs, &full).block {
				t.Errorf("%s: blocked early but not with body %q", tc.name, b)
			}
		}
	}
}
