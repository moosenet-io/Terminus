package wxgo

import "testing"

func TestSanitize(t *testing.T) {
	if Sanitize("  hi  ") != "hi" {
		t.Fatalf("sanitize should trim surrounding whitespace")
	}
}

func TestParsePositiveInt(t *testing.T) {
	if v, err := ParsePositiveInt(" 7 "); err != nil || v != 7 {
		t.Fatalf("want 7, got %d err %v", v, err)
	}
	if _, err := ParsePositiveInt("0"); err == nil {
		t.Fatalf("want error for 0")
	}
	if _, err := ParsePositiveInt("x"); err == nil {
		t.Fatalf("want error for non-integer")
	}
}
