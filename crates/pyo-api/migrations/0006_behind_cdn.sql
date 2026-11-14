-- The CDN in front of a service's websites ('cloudflare'): only it reaches
-- them, and the visitor is the address it forwards (pyo-proto's
-- firewall.rs). NULL: none.
ALTER TABLE pyo.services ADD COLUMN behind_cdn text
  CHECK (behind_cdn IN ('cloudflare'));
