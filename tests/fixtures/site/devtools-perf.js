function fib(n) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

// Keeps the main thread busy for 300 ms: a slow interaction and a clear CPU profile.
function busy() {
  const started = performance.now();
  let total = 0;
  while (performance.now() - started < 300) total += fib(20);
  document.getElementById('result').textContent = 'done ' + total;
}

// Never runs: shows up as unused bytes in coverage.
function neverCalled(items) {
  const totals = new Map();
  for (const item of items) {
    const key = item.category || 'other';
    totals.set(key, (totals.get(key) || 0) + item.price * item.quantity);
  }
  return [...totals.entries()].sort((a, b) => b[1] - a[1]).map(([key, value]) => `${key}: ${value.toFixed(2)}`);
}
