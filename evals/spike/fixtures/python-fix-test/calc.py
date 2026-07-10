"""SPIKE FIXTURE (throwaway). Defect: `factorial` has an off-by-one — it stops the product
at n-1 instead of n, so factorial(5) returns 24 not 120. Task: make `pytest` pass by fixing
the loop bound. The test encodes the contract; do not change it."""


def factorial(n: int) -> int:
    """Return n! for n >= 0."""
    if n < 0:
        raise ValueError("n must be non-negative")
    result = 1
    for i in range(1, n):  # DEFECT: should be range(1, n + 1)
        result *= i
    return result
