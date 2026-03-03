function requiredFloat(key: string, opts?: { min?: number; max?: number }): number {
  const value = process.env[key];
  if (value === undefined || value === "") {
    throw new Error(`Missing required environment variable: ${key}`);
  }
  const num = Number(value);
  if (isNaN(num)) {
    throw new Error(`Expected number for ${key}, got: ${value}`);
  }
  if (opts?.min !== undefined && num < opts.min) {
    throw new Error(`${key} must be >= ${opts.min}`);
  }
  if (opts?.max !== undefined && num > opts.max) {
    throw new Error(`${key} must be <= ${opts.max}`);
  }
  return num;
}
