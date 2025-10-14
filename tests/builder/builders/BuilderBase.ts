import { z } from "zod";

export class BuilderBase<T extends z.ZodTypeAny> {
  protected data: Partial<z.infer<T>>;

  constructor(private schema: T, defaults?: Partial<z.infer<T>>) {
    this.data = { ...defaults };
  }

  with<K extends keyof z.infer<T>>(key: K, value: z.infer<T>[K]) {
    this.data[key] = value;
    return this;
  }

  build(): z.infer<T> {
    return this.schema.parse(this.data);
  }
}
