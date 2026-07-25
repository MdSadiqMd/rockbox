def fib(n: int) -> int:
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a


for i in (10, 20, 30, 50):
    print(f"fib({i:>2}) = {fib(i)}")
