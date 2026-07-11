// Package mathx has a planted defect (the task): Add computes an int64 but its signature
// returns int, so the package does not compile. Fix it so `go test ./...` passes. Do NOT
// change the test.
package mathx

// Add returns the sum of a and b.
func Add(a, b int) int {
	var sum int64 = int64(a) + int64(b)
	// BUG: cannot use sum (int64) as int in return statement.
	return sum
}
