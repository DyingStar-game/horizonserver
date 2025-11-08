import { Faker } from "@faker-js/faker/.";
import { BuilderBase } from "./BuilderBase";
import {
  coordinate3dSchema,
  Coordinate3dType,
} from "../model/gorc/gorcBase.ws.model";

export class Coordinate3dBuilder extends BuilderBase<
  typeof coordinate3dSchema
> {
  constructor(faker: Faker) {
    super(coordinate3dSchema, {
      x: faker.number.int(),
      y: faker.number.int(),
      z: faker.number.int(),
    });
  }

  withX(x: number): this {
    this.data.x = x;
    return this;
  }

  withY(y: number): this {
    this.data.y = y;
    return this;
  }

  withZ(z: number): this {
    this.data.z = z;
    return this;
  }

  withCoordinate(coord: Coordinate3dType): this {
    this.data = coord;
    return this;
  }
}
