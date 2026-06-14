# A Python script loaded from the oxbow filesystem.  Run with:  py /hello.py
print("hello from a .py file on oxbow!")

# generators
def primes(limit):
    sieve = [True] * limit
    for n in range(2, limit):
        if sieve[n]:
            yield n
            for m in range(n * n, limit, n):
                sieve[m] = False

print("primes < 40:", list(primes(40)))

# slicing, enumerate, dict/set comprehensions
words = "the quick brown fox jumps".split()
print("reversed:", words[::-1])
print("lengths:", {w: len(w) for w in words})
print("unique letters:", len({c for w in words for c in w}))

# a class with operator overloading
class Vec:
    def __init__(self, x, y):
        self.x, self.y = x, y
    def __add__(self, o):
        return Vec(self.x + o.x, self.y + o.y)
    def __repr__(self):
        return "Vec({}, {})".format(self.x, self.y)

print("vector sum:", Vec(1, 2) + Vec(3, 4))

# closures + map/filter
adder = lambda n: lambda x: x + n
add10 = adder(10)
print("map+filter:", list(filter(lambda x: x % 2 == 0, map(add10, range(5)))))
