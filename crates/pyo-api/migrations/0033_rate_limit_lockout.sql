-- Rate limits that lock out for longer each time (ratelimit.rs): a key
-- that used up its window again within a day waits twice as long as the
-- last time, up to its limit's cap. A successful sign-in starts over.
ALTER TABLE pyo.rate_limits ADD COLUMN strikes integer NOT NULL DEFAULT 0;
ALTER TABLE pyo.rate_limits ADD COLUMN blocked_until timestamptz;
