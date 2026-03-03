function optionalJson<T = unknown>(key: string): T | undefined {
  const value = process.env[key];
  if (value === undefined || value === "") return undefined;
  try {
    return JSON.parse(value) as T;
  } catch {
    throw new Error(`Invalid JSON for environment variable ${key}`);
  }
}
