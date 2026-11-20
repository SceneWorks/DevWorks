export const SOURCE_EXTENSIONS = ['.ts', '.tsx', '.js', '.jsx', '.mts', '.cts', '.mjs', '.cjs'];
export const SOURCE_EXTENSION = /\.(?:[jt]sx?|[mc][jt]s)$/i;
export const DECLARATION_EXTENSION = /\.d\.(?:ts|mts|cts)$/i;

export function compilerFile(file: string): boolean {
  return SOURCE_EXTENSION.test(file);
}
