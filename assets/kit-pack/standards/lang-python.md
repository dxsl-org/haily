---
name: lang-python
description: Python standards — type hints, docstrings, error handling, idioms to follow when writing or reviewing Python (3.12+ preferred).
when_to_use: When writing, reviewing, or fixing Python code.
domain: developer
kind: standard
specialists: []
---

# Python Standards (3.12+ preferred)

## Type hints (mandatory on public API)

- Type hints are the contract — runtime cannot enforce them, so be precise.
- Prefer abstract types in parameters (`Iterable`, `Mapping`), concrete in returns (`list`, `dict`).
- `Protocol` over inheritance for duck-typed interfaces.
- No bare `# type: ignore` — always `# type: ignore[code]  # reason: ...`.

## Docstrings (PEP 257, reST/Sphinx style)

- Required on every public module, class, function, method. First line one-sentence summary, blank line, then detail.
- Always document `:raises:` — Python has no checked exceptions, so callers depend on the docstring.
- Omit docstrings on `_private` helpers when name + signature are obvious. Omit `:type`/`:rtype` when annotations are present.

## Error handling

- Catch specific exceptions, never bare `except:`. Re-raise with context (`raise X from e`) rather than swallowing.
- Validate external input at the boundary; fail loudly on contract violations.

## Idioms

- Naming: `snake_case` functions/vars, `PascalCase` classes, `SCREAMING_SNAKE_CASE` constants.
- Prefer comprehensions and generators over manual loops where they read clearly; avoid mutable default arguments.
- Use `pathlib` over `os.path`, f-strings over `%`/`.format`.
- Lint (ruff/flake8) + type check (mypy/pyright) clean before done.
