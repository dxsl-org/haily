import * as assert from "assert";
import { slugify } from "./index";

// The slugify contract (do not change): lowercase, trim, collapse whitespace runs to single
// hyphens, and leave existing hyphens intact.
assert.strictEqual(slugify("Hello World"), "hello-world");
assert.strictEqual(slugify("  Trim  Me  "), "trim-me");
assert.strictEqual(slugify("Already-Slug"), "already-slug");

console.log("ok");
