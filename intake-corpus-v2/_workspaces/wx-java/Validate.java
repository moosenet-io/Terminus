// wx-java: config-field validators (derived from lumina vitals/config
// validation). Default package, no build tool — `javac *.java` + `java <Class>`.
public final class Validate {
    private Validate() {}

    /** Trim surrounding whitespace; reject null. */
    public static String sanitize(String s) {
        if (s == null) {
            throw new IllegalArgumentException("value is null");
        }
        return s.trim();
    }

    /** Parse a strictly-positive integer, or throw. */
    public static int parsePositiveInt(String s) {
        int v = Integer.parseInt(sanitize(s));
        if (v <= 0) {
            throw new IllegalArgumentException("not positive: " + v);
        }
        return v;
    }
}
