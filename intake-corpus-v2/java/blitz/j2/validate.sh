#!/usr/bin/env bash
# Validator for java-blitz-j2 (Validate.clampPercent). Stage contract per README.
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
    static boolean throwsOn(Runnable r){ try { r.run(); return false; } catch (RuntimeException e){ return true; } }
    public static void main(String[] a){
        check(Validate.sanitize("  hi  ").equals("hi"), "sanitize trims");
        check(Validate.parsePositiveInt(" 7 ")==7, "parsePositiveInt");
        check(throwsOn(() -> Validate.parsePositiveInt("0")), "parsePositiveInt rejects 0");
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
    static boolean throwsOn(String s){ try { Validate.clampPercent(s); return false; } catch (RuntimeException e){ return true; } }
    public static void main(String[] a){
        check(Validate.clampPercent("0.5") == 0.5, "0.5");
        check(Validate.clampPercent("0") == 0.0, "lower bound 0");
        check(Validate.clampPercent("1") == 1.0, "upper bound 1");
        check(throwsOn("1.5"), "above range");
        check(throwsOn("-0.1"), "below range");
        check(throwsOn("x"), "non-numeric");
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
