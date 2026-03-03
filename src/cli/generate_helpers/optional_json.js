function optionalJson(key: string): unknown {
  const value = process.env[key];
  if (value === undefined || value === "") return undefined;
  try {
    return JSON.parse(value);
  } catch {
    throw new Error(`Invalid JSON for environment variable ${key}`);
  }
}
