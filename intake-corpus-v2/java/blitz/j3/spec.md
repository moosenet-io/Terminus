## Task
Create a new `Slug.java` utility in the `wx-java` workspace that turns arbitrary
text into a URL-style slug.

## Description
Add a new class `Slug` (default package) exposing a single static method
`public static String slugify(String s)`.

Slugify rules:
- lowercase the input,
- replace every maximal run of characters that are NOT `[a-z0-9]` with a single
  `-` (so letters/digits are kept, everything else — spaces, punctuation — is a
  separator),
- strip any leading and trailing `-`.

## FILES
- `Slug.java` — the new class with the `slugify` method.

## APPROACH
Lowercase first, then collapse non-alphanumeric runs to a single `-` (a regex
such as `[^a-z0-9]+` → `-` works well), then trim leading/trailing `-`.

## TEST PLAN
- `"Hello, World!"` → `"hello-world"`.
- `"  Foo   Bar  "` → `"foo-bar"`.
- `"a--b"` → `"a-b"`.
- `"!!!"` → `""` (empty).
