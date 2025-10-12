import { Faker, faker } from "@faker-js/faker";

export class FakerGeneratorFactory {
  private static instance?: Faker;
  private static currentSeed: number;

  public static getInstance(): Faker {
    if (!this.instance) {
      this.init();
    }
    return this.instance!;
  }

  public static getSeed(): number {
    return this.currentSeed;
  }

  public static init(): void {
    this.currentSeed = process.env.FAKER_SEED
      ? Number(process.env.FAKER_SEED)
      : Math.floor(10000 + Math.random() * 90000);

    faker.seed(this.currentSeed);
    this.instance = faker;

    console.log(
      `To reproduce the test with the same seed set the FAKER_SEED environment variable to ${this.currentSeed}`
    );
  }
}
