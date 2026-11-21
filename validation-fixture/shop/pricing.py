def apply_discount(price: float, percent: float) -> float:
    """Return price reduced by `percent`, given as a whole-number percentage (0-100)."""
    if percent < 0 or percent > 100:
        raise ValueError("percent must be between 0 and 100")
    return price - price * percent / 100
