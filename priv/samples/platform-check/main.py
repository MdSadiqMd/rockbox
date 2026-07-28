import platform

print("=== rockbox platform check ===")
print(f"language: python")
print(f"version: {platform.python_version()}")
print(f"implementation: {platform.python_implementation()}")

n = 20
a, b = 0, 1
for _ in range(n):
    a, b = b, a + b
print(f"fib({n}) = {a}")

print("=== all checks passed ===")
