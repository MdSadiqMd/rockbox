async function delay<T>(ms: number, value: T): Promise<T> {
  return new Promise((resolve) => setTimeout(() => resolve(value), ms));
}

async function main(): Promise<void> {
  const results = await Promise.all([
    delay(50, "first"),
    delay(30, "second"),
    delay(70, "third"),
  ]);
  console.log("settled order:", results.join(", "));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
