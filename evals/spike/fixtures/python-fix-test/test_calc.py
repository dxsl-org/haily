"""Contract for calc.factorial — do not modify (the fix belongs in calc.py)."""
import pytest

from calc import factorial


def test_small():
    assert factorial(0) == 1
    assert factorial(1) == 1
    assert factorial(5) == 120


def test_negative_raises():
    with pytest.raises(ValueError):
        factorial(-1)
