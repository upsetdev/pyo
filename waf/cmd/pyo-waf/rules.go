package main

// Custom rules: Pyo's users' own tests on a request, run before the OWASP
// rules. They're evaluated here, as data: every value is compared as a
// plain string (or compiled as an RE2 regex, which runs in linear time), so
// nothing in a rule can become a Coraza directive or change another rule.

import (
	"fmt"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"regexp"
	"strings"

	"github.com/oschwald/maxminddb-golang"
)

// Rule is one custom rule as Pyo sends it.
type Rule struct {
	ID         int    `json:"id"`
	Name       string `json:"name"`
	Match      string `json:"match"` // "all" or "any"
	Conditions []Cond `json:"conditions"`
	// "block" (403), "log", "allow" (no later rule, no OWASP rules) or
	// "skipOwasp" (later custom rules, no OWASP rules).
	Action string `json:"action"`
}

// Cond is one test.
type Cond struct {
	Field  string   `json:"field"`
	Name   string   `json:"name"`
	Op     string   `json:"op"`
	Values []string `json:"values"`
	Negate bool     `json:"negate"`
}

type cond struct {
	Cond
	set  map[string]bool
	nets []netip.Prefix
	re   *regexp.Regexp
}

type rule struct {
	id     int
	name   string
	any    bool
	conds  []cond
	action string
}

// Limits, as Pyo checks them; a rule past them is dropped.
const (
	maxConditions = 10
	maxValues     = 50
	maxValue      = 256
)

var fields = map[string]bool{"path": true, "query": true, "method": true, "host": true, "header": true, "userAgent": true, "cookie": true, "body": true, "ip": true, "country": true}

// compileRule checks and prepares one rule.
func compileRule(r Rule) (rule, error) {
	out := rule{id: r.ID, name: r.Name, any: r.Match == "any", action: r.Action}
	switch r.Action {
	case "block", "log", "allow", "skipOwasp":
	default:
		return out, fmt.Errorf("rule %d: action %q", r.ID, r.Action)
	}
	if r.Match != "all" && r.Match != "any" {
		return out, fmt.Errorf("rule %d: match %q", r.ID, r.Match)
	}
	if len(r.Conditions) == 0 || len(r.Conditions) > maxConditions {
		return out, fmt.Errorf("rule %d: %d conditions", r.ID, len(r.Conditions))
	}
	for _, c := range r.Conditions {
		if !fields[c.Field] {
			return out, fmt.Errorf("rule %d: field %q", r.ID, c.Field)
		}
		if len(c.Values) == 0 || len(c.Values) > maxValues || (c.Op != "in" && len(c.Values) != 1) {
			return out, fmt.Errorf("rule %d: %d values for %s", r.ID, len(c.Values), c.Op)
		}
		for _, v := range c.Values {
			if len(v) > maxValue {
				return out, fmt.Errorf("rule %d: a value is too long", r.ID)
			}
		}
		x := cond{Cond: c}
		switch c.Op {
		case "equals", "contains", "startsWith", "endsWith":
		case "in":
			x.set = map[string]bool{}
			for _, v := range c.Values {
				x.set[v] = true
				if c.Field == "ip" {
					p, err := netip.ParsePrefix(v)
					if err != nil {
						a, err2 := netip.ParseAddr(v)
						if err2 != nil {
							return out, fmt.Errorf("rule %d: %q isn't an address or range", r.ID, v)
						}
						p = netip.PrefixFrom(a, a.BitLen())
					}
					x.nets = append(x.nets, p.Masked())
				}
			}
		case "regex":
			re, err := regexp.Compile(c.Values[0])
			if err != nil {
				return out, fmt.Errorf("rule %d: %w", r.ID, err)
			}
			x.re = re
		default:
			return out, fmt.Errorf("rule %d: operator %q", r.ID, c.Op)
		}
		if c.Field == "ip" && c.Op != "in" {
			return out, fmt.Errorf("rule %d: ip takes \"in\"", r.ID)
		}
		out.conds = append(out.conds, x)
	}
	return out, nil
}

// request is what conditions read, filled in as they need it.
type request struct {
	method, host, uri string
	headers           map[string]string
	body              []byte
	client            string
	geo               *maxminddb.Reader

	cookies map[string]string
	country *string
}

func (r *request) path() string {
	p := r.uri
	if i := strings.IndexAny(p, "?#"); i >= 0 {
		p = p[:i]
	}
	// Compared decoded, so "/%77p-admin" is "/wp-admin".
	if d, err := url.PathUnescape(p); err == nil {
		return d
	}
	return p
}

func (r *request) query() string {
	q := r.uri
	if i := strings.IndexByte(q, '?'); i >= 0 {
		q = q[i+1:]
		if j := strings.IndexByte(q, '#'); j >= 0 {
			q = q[:j]
		}
		return q
	}
	return ""
}

func (r *request) cookie(name string) string {
	if r.cookies == nil {
		r.cookies = map[string]string{}
		if cs, err := http.ParseCookie(r.headers["cookie"]); err == nil {
			for _, c := range cs {
				if _, ok := r.cookies[c.Name]; !ok {
					r.cookies[c.Name] = c.Value
				}
			}
		}
	}
	return r.cookies[name]
}

func (r *request) countryCode() string {
	if r.country != nil {
		return *r.country
	}
	cc := ""
	if ip := net.ParseIP(r.client); ip != nil && r.geo != nil {
		var rec struct {
			Country struct {
				ISOCode string `maxminddb:"iso_code"`
			} `maxminddb:"country"`
		}
		if r.geo.Lookup(ip, &rec) == nil {
			cc = rec.Country.ISOCode
		}
	}
	r.country = &cc
	return cc
}

func (r *request) value(c *cond) string {
	switch c.Field {
	case "path":
		return r.path()
	case "query":
		return r.query()
	case "method":
		return strings.ToUpper(r.method)
	case "host":
		return r.host
	case "header":
		return r.headers[c.Name]
	case "userAgent":
		return r.headers["user-agent"]
	case "cookie":
		return r.cookie(c.Name)
	case "body":
		return string(r.body)
	case "country":
		return r.countryCode()
	}
	return ""
}

func (c *cond) test(r *request) bool {
	if c.Field == "ip" {
		a, err := netip.ParseAddr(r.client)
		if err != nil {
			return false
		}
		a = a.Unmap()
		for _, p := range c.nets {
			if p.Contains(a) {
				return true
			}
		}
		return false
	}
	v := r.value(c)
	if c.Field == "country" && v == "" {
		return false // unknown: no country test matches it
	}
	switch c.Op {
	case "equals":
		return v == c.Values[0]
	case "contains":
		return strings.Contains(v, c.Values[0])
	case "startsWith":
		return strings.HasPrefix(v, c.Values[0])
	case "endsWith":
		return strings.HasSuffix(v, c.Values[0])
	case "in":
		return c.set[v]
	case "regex":
		return c.re.MatchString(v)
	}
	return false
}

func (r rule) matches(req *request) bool {
	for i := range r.conds {
		c := &r.conds[i]
		ok := c.test(req) != c.Negate
		if r.any && ok {
			return true
		}
		if !r.any && !ok {
			return false
		}
	}
	return !r.any
}

// outcome of a host's custom rules for one request.
type outcome struct {
	matched   []*rule
	block     bool
	skipOwasp bool
}

func evaluate(rules []rule, req *request) outcome {
	var o outcome
	for i := range rules {
		r := &rules[i]
		if !r.matches(req) {
			continue
		}
		o.matched = append(o.matched, r)
		switch r.action {
		case "block":
			o.block = true
			return o
		case "allow":
			o.skipOwasp = true
			return o
		case "skipOwasp":
			o.skipOwasp = true
		}
	}
	return o
}

// Before the body: what a host's custom rules decide from the request's
// line, headers, address and country alone, so a request they block is
// refused at once instead of waiting for its body. A rule with a body
// condition that the rest doesn't settle is undecided; evaluation stops
// short of anything an undecided rule could change, so an early outcome
// is always the one the whole request gets.

// tri: a test's result before the body; unknown when it depends on it.
type tri int

const (
	no tri = iota
	yes
	unknown
)

func (r rule) matchesEarly(req *request) tri {
	out := yes
	if r.any {
		out = no
	}
	for i := range r.conds {
		c := &r.conds[i]
		var t tri
		switch {
		case c.Field == "body":
			t = unknown
		case c.test(req) != c.Negate:
			t = yes
		default:
			t = no
		}
		switch {
		case r.any && t == yes:
			return yes
		case !r.any && t == no:
			return no
		case t == unknown:
			out = unknown
		}
	}
	return out
}

// evaluateEarly: the custom rules' outcome before the body. block is set
// only when the whole request is certain to be blocked by them; owasp is
// true when they certainly leave the OWASP rules to run (no "allow" or
// "skipOwasp" matched or undecided).
func evaluateEarly(rules []rule, req *request) (o outcome, owasp bool) {
	owasp = true
	maybeAllowed := false // an undecided "allow" before: nothing after is certain
	for i := range rules {
		r := &rules[i]
		switch r.matchesEarly(req) {
		case no:
			continue
		case unknown:
			switch r.action {
			case "allow":
				maybeAllowed = true
				owasp = false
			case "skipOwasp":
				owasp = false
			}
			continue
		}
		o.matched = append(o.matched, r)
		switch r.action {
		case "block":
			o.block = !maybeAllowed
			return o, owasp && !maybeAllowed
		case "allow":
			o.skipOwasp = true
			return o, false
		case "skipOwasp":
			o.skipOwasp = true
			owasp = false
		}
	}
	return o, owasp
}
