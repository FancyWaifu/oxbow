// A JavaScript file loaded from the oxbow filesystem.  Run with:  js /hello.js
print("hello from a .js file on oxbow!");

// generators
function* primes(limit) {
  const sieve = new Array(limit).fill(true);
  for (let n = 2; n < limit; n++) {
    if (sieve[n]) {
      yield n;
      for (let m = n * n; m < limit; m += n) sieve[m] = false;
    }
  }
}
print("primes < 40:", [...primes(40)].join(" "));

// destructuring, Map, Set, spread
const [a, b, ...rest] = [1, 2, 3, 4, 5];
print("destructure:", a, b, "rest:", JSON.stringify(rest));
const m = new Map([["x", 1], ["y", 2]]);
print("map x+y:", m.get("x") + m.get("y"));
print("unique:", [...new Set([1, 1, 2, 3, 3, 3])].join(","));

// higher-order + closures
const compose = (f, g) => x => f(g(x));
const inc = x => x + 1, dbl = x => x * 2;
print("compose:", compose(inc, dbl)(10));

// optional chaining + nullish coalescing
const obj = { a: { b: 42 } };
print("opt chain:", obj?.a?.b, obj?.z?.w ?? "default");

// tagged template + reduce
const nums = [1, 2, 3, 4, 5];
print("sum/prod:", nums.reduce((a, b) => a + b), nums.reduce((a, b) => a * b));
