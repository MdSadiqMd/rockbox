const numbers = Array.from({ length: 10 }, (_, i) => i + 1);
const sum = numbers.reduce((acc, n) => acc + n, 0);
console.log("hello from typescript");
console.log(`sum of 1..10 = ${sum}`);
