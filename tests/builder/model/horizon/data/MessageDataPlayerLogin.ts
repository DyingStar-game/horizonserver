import { MessageDataInterface } from "./MessageDataInterface";

export class MessageDataPlayerLogin implements MessageDataInterface {
  public login: string;
  public password: string;
  constructor(login: string, password: string) {
    this.login = login;
    this.password = password;
  }
}
