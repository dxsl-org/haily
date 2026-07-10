// Contract for slugify — do not modify (the implementation belongs in index.ts).
import assert from "node:assert/strict";
import { slugify } from "./index.js";

assert.equal(slugify("Hello World"), "hello-world");
assert.equal(slugify("  Foo_Bar  "), "foo-bar");
assert.equal(slugify("Rust & TypeScript!"), "rust-typescript");
assert.equal(slugify("already-a-slug"), "already-a-slug");
assert.equal(slugify("multiple   spaces"), "multiple-spaces");

console.log("ok: slugify contract satisfied");
