import { describe, expect, test } from "bun:test";
import { check, contractOf, unsafeChanges } from "./check-migrations";

const added = (file: string, sql: string) => ({ status: "A", file, sql });
const repo = { existing: ["0001_init.sql", "0033_x.sql"], tags: ["v0.3.0"] };

describe("what breaks the previous release", () => {
  test.each([
    ["DROP TABLE pyo.old;"],
    ["drop table if exists pyo.old;"],
    ["ALTER TABLE pyo.users DROP COLUMN nickname;"],
    ["ALTER TABLE pyo.users RENAME COLUMN a TO b;"],
    ["ALTER TABLE pyo.a RENAME TO b;"],
    ["ALTER SCHEMA app RENAME TO pyo;"],
    ["ALTER TABLE pyo.users ALTER COLUMN age TYPE bigint;"],
    ["ALTER TABLE pyo.users ALTER age SET DATA TYPE bigint;"],
    ["ALTER TABLE pyo.users ALTER COLUMN name SET NOT NULL;"],
    ["TRUNCATE pyo.sessions;"],
    ["CREATE UNIQUE INDEX a ON pyo.users (email);"],
    ["ALTER TABLE pyo.users ADD CONSTRAINT one UNIQUE (email);"],
    ["ALTER TABLE pyo.users ADD COLUMN age int NOT NULL;"],
    ["ALTER TABLE pyo.users ADD age int NOT NULL;"],
  ])("%s", (sql) => expect(unsafeChanges(sql)).not.toEqual([]));
});

describe("what it keeps working with", () => {
  test.each([
    ["ALTER TABLE pyo.users ADD COLUMN must_set_up boolean NOT NULL DEFAULT false;"],
    ["ALTER TABLE pyo.users ADD COLUMN nickname text;"],
    ["CREATE TABLE pyo.new (id text PRIMARY KEY, name text NOT NULL);"],
    ["CREATE INDEX a ON pyo.users (email);"],
    ["UPDATE pyo.settings SET value = '{}' WHERE key = 'x';"],
    ["-- DROP TABLE pyo.users;\nSELECT 1;"],
    ["/* TRUNCATE pyo.users; */ SELECT 1;"],
    ["INSERT INTO pyo.notes (body) VALUES ('please DROP TABLE and RENAME it');"],
  ])("%s", (sql) => expect(unsafeChanges(sql)).toEqual([]));
});

describe("contracts", () => {
  const drop = "ALTER TABLE pyo.users DROP COLUMN nickname;";

  test("a contract reads its version and reason", () => {
    expect(contractOf("-- pyo:contract since=v0.3.0 nothing reads it\nSELECT 1;")).toEqual({ since: "v0.3.0", reason: "nothing reads it" });
  });

  test("a contract needs a reason", () => {
    expect(contractOf("-- pyo:contract since=v0.3.0\nSELECT 1;")).toBeNull();
  });

  test("a removal without one fails", () => {
    expect(check([added("0034_a.sql", drop)], repo)).toHaveLength(1);
  });

  test("a removal naming a released version passes", () => {
    const sql = `-- pyo:contract since=v0.3.0 nothing reads it\n${drop}`;
    expect(check([added("0034_a.sql", sql)], repo)).toEqual([]);
  });

  test("a removal naming an unreleased version fails", () => {
    const sql = `-- pyo:contract since=v9.9.9 nothing reads it\n${drop}`;
    expect(check([added("0034_a.sql", sql)], repo)[0]).toContain("isn't a released tag");
  });
});

describe("the migration files themselves", () => {
  test("editing an applied migration fails", () => {
    expect(check([{ status: "M", file: "0001_init.sql", sql: "" }], repo)[0]).toContain("can't be changed");
  });

  test("deleting one fails", () => {
    expect(check([{ status: "D", file: "0001_init.sql", sql: "" }], repo)[0]).toContain("can't be changed");
  });

  test("a number at or below the latest fails", () => {
    expect(check([added("0033_y.sql", "SELECT 1;")], repo).join()).toContain("above 0033");
  });

  test("two new migrations can't share a number", () => {
    const problems = check([added("0034_a.sql", "SELECT 1;"), added("0034_b.sql", "SELECT 1;")], repo);
    expect(problems.join()).toContain("another migration has this number");
  });

  test("a badly named file fails", () => {
    expect(check([added("add_users.sql", "SELECT 1;")], repo)[0]).toContain("name it like");
  });
});

describe("before the first release", () => {
  const before = { existing: ["0001_init.sql", "0002_x.sql", "0003_y.sql"], tags: [] };
  const fold = [
    { status: "M", file: "0001_init.sql", sql: "CREATE SCHEMA pyo;\n" },
    { status: "D", file: "0002_x.sql", sql: "" },
    { status: "D", file: "0003_y.sql", sql: "" },
  ];

  test("migrations can still be edited and merged", () => {
    expect(check(fold, before)).toEqual([]);
  });

  test("a new migration numbers on from what's left", () => {
    expect(check([...fold, added("0002_rate_limits.sql", "CREATE TABLE pyo.r (id text);")], before)).toEqual([]);
    expect(check([...fold, added("0001_again.sql", "SELECT 1;")], before).join()).toContain("above 0001");
  });

  test("new migrations still have to keep the previous build working", () => {
    expect(check([added("0004_a.sql", "ALTER TABLE pyo.users DROP COLUMN nickname;")], before)).toHaveLength(1);
  });

  test("once a release is tagged, they're frozen", () => {
    expect(check(fold, { ...before, tags: ["v0.1.0"] }).join()).toContain("can't be changed");
  });
});
