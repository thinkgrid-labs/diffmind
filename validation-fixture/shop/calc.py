import ast


def evaluate(expr: str) -> float:
    """Evaluate a numeric literal safely."""
    return float(ast.literal_eval(expr))
