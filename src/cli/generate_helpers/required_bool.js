function requiredBool(key: string): boolean {
  const value = process.env[key]?.toLowerCase();
  if (value === undefined || value === "") {
    throw new Error(`Missing required environment variable: ${key}`);
  }
  return ["true", "1", "yes"].includes(value);
}
