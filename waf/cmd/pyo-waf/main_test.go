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

// A path of a shared hostname goes by its own settings: the longest path a
// request is under, by whole segments, else the hostname's.
func TestPathsGoByTheirOwnSettings(t *testing.T) {
	p := filepath.Join(t.TempDir(), "config.json")
	os.WriteFile(p, []byte(`{"sets":{"a":{"engine":"On","level":1},"b":{"engine":"DetectionOnly","level":1}},
		"hosts":{"Shop.Example.com":"a","Shop.Example.com/api":"b"},
		"lists":{"r":[{"id":1,"name":"x","match":"all","action":"block","conditions":[{"field":"path","op":"contains","values":["x"]}]}]},
		"hostRules":{"shop.example.com/api/v2":"r"}}`), 0o600)
	c, _, err := load(p, nil)
	if err != nil {
		t.Fatal(err)
	}
	for path, want := range map[string]string{
		"/":              "shop.example.com",
		"/apix":          "shop.example.com",
		"/api":           "shop.example.com/api",
		"/api/x?q=/api2": "shop.example.com/api",
		"/api/v2/y":      "shop.example.com/api/v2",
	} {
		if got := c.key("shop.example.com", path); got != want {
			t.Errorf("%s: %s, want %s", path, got, want)
		}
	}
	if c.blocking["shop.example.com/api"] || !c.blocking["shop.example.com"] {
		t.Error("each path keeps its own engine")
	}
	if c.key("other.example.com", "/api") != "other.example.com" {
		t.Error("other hostnames go by their name")
	}
}
