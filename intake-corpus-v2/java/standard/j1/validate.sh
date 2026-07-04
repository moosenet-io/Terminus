#!/usr/bin/env bash
# Validator for java-standard-j1 (new RingBuffer). Stage contract per README.
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
import java.util.Arrays;
public class MintCheck {
    static void check(boolean c, String m){ if(!c) throw new AssertionError(m); }
    static boolean throwsOn(int cap){ try { new RingBuffer(cap); return false; } catch (RuntimeException e){ return true; } }
    public static void main(String[] a){
        RingBuffer rb = new RingBuffer(3);
        check(rb.size()==0, "fresh size 0");
        check(rb.capacity()==3, "capacity");
        check(rb.toArray().length==0, "fresh empty");
        rb.push(1); rb.push(2); rb.push(3);
        check(Arrays.equals(rb.toArray(), new int[]{1,2,3}), "filled in order");
        check(rb.size()==3, "size 3");
        rb.push(4);
        check(Arrays.equals(rb.toArray(), new int[]{2,3,4}), "overwrote oldest");
        check(rb.size()==3, "still 3");
        check(throwsOn(0), "capacity 0 rejected");
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
