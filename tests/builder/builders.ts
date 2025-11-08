import { Faker } from "@faker-js/faker/.";
import { FakerGeneratorFactory } from "./FakerGeneratorFactory";
import { PlayerLoginWsBuilder } from "./builders/PlayerLoginWsBuilder";
import { Coordinate3dBuilder } from "./builders/Coordinate3dBuilder";
import { MovePlayerEventWsBuilder } from "./builders/gorc/MovePlayerEventWsBuilder";

export const aPlayerLoginWs = (generator?: Faker): PlayerLoginWsBuilder => {
  const faker = generator ?? FakerGeneratorFactory.getInstance();

  return new PlayerLoginWsBuilder(faker);
};

export const aCoordinate3d = (generator?: Faker): Coordinate3dBuilder => {
  const faker = generator ?? FakerGeneratorFactory.getInstance();

  return new Coordinate3dBuilder(faker);
};

export const aMovePlayerEventWs = (
  generator?: Faker
): MovePlayerEventWsBuilder => {
  const faker = generator ?? FakerGeneratorFactory.getInstance();

  return new MovePlayerEventWsBuilder(faker);
};
