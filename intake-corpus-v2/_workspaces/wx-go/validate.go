// Package wxgo holds small, stdlib-only config-field validators (derived from
// lumina vitals/config validation).
package wxgo

import (
	"fmt"
	"strconv"
	"strings"
)

// Sanitize trims surrounding whitespace.
func Sanitize(s string) string {
	return strings.TrimSpace(s)
}

// ParsePositiveInt parses a strictly-positive integer or returns an error.
func ParsePositiveInt(s string) (int, error) {
	v, err := strconv.Atoi(Sanitize(s))
	if err != nil {
		return 0, fmt.Errorf("not an integer: %q", s)
	}
	if v <= 0 {
		return 0, fmt.Errorf("not positive: %d", v)
	}
	return v, nil
}
