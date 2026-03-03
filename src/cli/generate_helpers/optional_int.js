function optionalInt(key: string, opts?: { allowed?: number[]; min?: number; max?: number }): number | undefined {
  const value = process.env[key];
  if (value === undefined || value === "") return undefined;
  const num = Number(value);
  if (!Number.isInteger(num)) {
    throw new Error(`Expected integer for ${key}, got: ${value}`);
  }
  if (opts?.allowed && !opts.allowed.includes(num)) {
    throw new Error(`${key} must be one of: ${opts.allowed.join(", ")}`);
  }
  if (opts?.min !== undefined && num < opts.min) {
    throw new Error(`${key} must be >= ${opts.min}`);
  }
  if (opts?.max !== undefined && num > opts.max) {
    throw new Error(`${key} must be <= ${opts.max}`);
  }
  return num;
}
