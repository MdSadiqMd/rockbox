from greeter import greet
from math_utils import factorial

greet("Rockbox")
for n in (5, 10, 12):
    print(f"{n}! = {factorial(n)}")
