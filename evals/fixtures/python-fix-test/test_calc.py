from calc import factorial


def test_factorial_base_case():
    assert factorial(0) == 1


def test_factorial_recursive():
    assert factorial(5) == 120
    assert factorial(3) == 6
