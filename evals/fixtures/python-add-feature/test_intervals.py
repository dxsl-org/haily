from intervals import merge_intervals


def test_empty():
    assert merge_intervals([]) == []


def test_overlapping_and_adjacent():
    assert merge_intervals([(1, 3), (2, 6), (8, 10), (15, 18)]) == [(1, 6), (8, 10), (15, 18)]
    assert merge_intervals([(1, 4), (4, 5)]) == [(1, 5)]


def test_unsorted_input():
    assert merge_intervals([(8, 10), (1, 3), (2, 6)]) == [(1, 6), (8, 10)]
