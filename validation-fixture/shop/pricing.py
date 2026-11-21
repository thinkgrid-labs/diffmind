def apply_discount(price: float, percent: float) -> float:
    """Return price reduced by `percent`, given as a whole-number percentage (0-100)."""
    if percent < 0 or percent > 100:
        raise ValueError("percent must be between 0 and 100")
    return price - price * percent / 100


def clamp_percent(percent: float) -> float:
    """Clamp `percent` into the 0-100 range accepted by apply_discount."""
    return max(0.0, min(100.0, percent))
