function requiredJson<T = unknown>(key: string): T {
  const value = process.env[key];
  if (value === undefined || value === "") {
    throw new Error(`Missing required environment variable: ${key}`);
  }
  try {
    return JSON.parse(value) as T;
  } catch {
    throw new Error(`Invalid JSON for environment variable ${key}`);
  }
}
