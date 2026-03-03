function optionalBool(key: string): boolean | undefined {
  const value = process.env[key]?.toLowerCase();
  if (value === undefined || value === "") return undefined;
  return ["true", "1", "yes"].includes(value);
}
