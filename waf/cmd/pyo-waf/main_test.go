package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestDirectives(t *testing.T) {
	d := directives(Set{Engine: "DetectionOnly", Level: 3, Exceptions: []int{942100, 941100}})
	for _, want := range []string{"SecRuleEngine DetectionOnly", "blocking_paranoia_level=3", "SecRuleRemoveById 942100", "SecRuleRemoveById 941100"} {
		if !strings.Contains(d, want) {
			t.Errorf("missing %q in\n%s", want, d)
		}
	}
	// Anything but "DetectionOnly" blocks; out-of-range levels are 1.
	if d := directives(Set{Engine: "x", Level: 9}); !strings.Contains(d, "SecRuleEngine On") || !strings.Contains(d, "blocking_paranoia_level=1") {
		t.Errorf("defaults:\n%s", d)
	}
}

func TestLoadSharesSets(t *testing.T) {
	p := filepath.Join(t.TempDir(), "config.json")
	os.WriteFile(p, []byte(`{"sets":{"a":{"engine":"On","level":1}},"hosts":{"Shop.Example.com":"a","b.example.com":"a","c.example.com":"missing"}}`), 0o600)
	c, cache, err := load(p, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(c.hosts) != 2 || c.hosts["shop.example.com"] == nil || c.hosts["shop.example.com"] != c.hosts["b.example.com"] {
		t.Fatalf("hosts: %v", c.names)
	}
	// Reloading the same settings reuses the compiled rules.
	c2, _, err := load(p, cache)
	if err != nil || c2.hosts["b.example.com"] != c.hosts["b.example.com"] {
		t.Fatal("not reused")
	}
}

func TestThreshold(t *testing.T) {
	if d := directives(Set{Engine: "On", Level: 1, Threshold: 20}); !strings.Contains(d, "inbound_anomaly_score_threshold=20") {
		t.Errorf("no threshold:\n%s", d)
	}
	if d := directives(Set{Engine: "On", Level: 1}); strings.Contains(d, "inbound_anomaly_score_threshold") {
		t.Errorf("default threshold should be the rule set's own:\n%s", d)
	}
}
