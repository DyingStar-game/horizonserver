import { Faker } from "@faker-js/faker/.";
import { MessageDataPlayerLogin } from "../../model/horizon/data/MessageDataPlayerLogin";

export class MessageDataPlayerLoginBuilder {
  private login: string;
  private password: string;

  constructor(faker: Faker) {
    this.login = faker.internet.username();
    this.password = faker.internet.password();
  }

  public build(): MessageDataPlayerLogin {
    return new MessageDataPlayerLogin(this.login, this.password);
  }

  public withLogin(login: string): this {
    this.login = login;
    return this;
  }

  public withPassword(password: string): this {
    this.password = password;
    return this;
  }
}
