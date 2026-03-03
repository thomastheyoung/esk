function requiredJson(key: string): unknown {
  const value = process.env[key];
  if (value === undefined || value === "") {
    throw new Error(`Missing required environment variable: ${key}`);
  }
  try {
    return JSON.parse(value);
  } catch {
    throw new Error(`Invalid JSON for environment variable ${key}`);
  }
}
