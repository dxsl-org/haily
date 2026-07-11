"""Planted defect (the task): factorial has a wrong base case, so every result collapses to 0.
Fix it so the tests in test_calc.py pass. Do NOT change the tests."""


def factorial(n: int) -> int:
    # BUG: the base case should return 1, not 0.
    if n == 0:
        return 0
    return n * factorial(n - 1)
