#!/usr/bin/env bash
# Validator for java-blitz-j3 (new Slug.slugify). Stage contract per README.
set -uo pipefail
WORK="${MINT_WORK:-$PWD}"
cd "$WORK" || { echo "STAGE:COMPILE fail"; echo "FAIL"; exit 1; }

command -v javac >/dev/null 2>&1 || { echo "TOOLCHAIN:missing javac"; exit 3; }
command -v java  >/dev/null 2>&1 || { echo "TOOLCHAIN:missing java";  exit 3; }

OUT="$WORK/_classes"; rm -rf "$OUT"; mkdir -p "$OUT"

if javac -d "$OUT" *.java >/tmp/mint_jc.$$ 2>&1; then
  echo "STAGE:COMPILE ok"
else
  echo "STAGE:COMPILE fail"; cat /tmp/mint_jc.$$ >&2; echo "FAIL"; exit 1
fi

cat > "$WORK/MintBase.java" <<'JAVA'
public class MintBase {
    static void check(boolean c, String m){ if(!c) throw new AssertionError(m); }
    public static void main(String[] a){
        check(Validate.sanitize("  hi  ").equals("hi"), "sanitize trims");
        check(Validate.parsePositiveInt(" 7 ")==7, "parsePositiveInt");
        System.out.println("BASE-OK");
    }
}
JAVA
if javac -d "$OUT" -cp "$OUT" "$WORK/MintBase.java" >/tmp/mint_jb.$$ 2>&1 && \
   java -cp "$OUT" MintBase >/dev/null 2>&1; then
  echo "STAGE:TESTS ok"
else
  echo "STAGE:TESTS fail"; cat /tmp/mint_jb.$$ >&2
fi

cat > "$WORK/MintCheck.java" <<'JAVA'
public class MintCheck {
    static void check(boolean c, String m){ if(!c) throw new AssertionError(m); }
    public static void main(String[] a){
        check(Slug.slugify("Hello, World!").equals("hello-world"), "punct+space");
        check(Slug.slugify("  Foo   Bar  ").equals("foo-bar"), "leading/trailing + runs");
        check(Slug.slugify("a--b").equals("a-b"), "collapse separators");
        check(Slug.slugify("!!!").equals(""), "all separators -> empty");
        System.out.println("CHANGE-OK");
    }
}
JAVA
if javac -d "$OUT" -cp "$OUT" "$WORK/MintCheck.java" >/tmp/mint_jk.$$ 2>&1 && \
   java -cp "$OUT" MintCheck >/dev/null 2>&1; then
  echo "STAGE:CHANGE ok"; echo "PASS"; exit 0
else
  echo "STAGE:CHANGE fail"; cat /tmp/mint_jk.$$ >&2; echo "FAIL"; exit 1
fi
