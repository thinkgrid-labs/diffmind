/// <reference types="node" />
/**
 * Benchmark file for AI Code Review
 */
export class UserManager {
  private users: Map<string, any> = new Map();

  async addUser(id: string, data: any) {
    // Potential security risk: no validation
    this.users.set(id, data);
    console.log(`User ${id} added`);
  }

  async getUser(id: string) {
    return this.users.get(id);
  }

  // Performance bottleneck: O(n) search where O(1) is possible
  findUserByEmail(email: string) {
    for (const [id, user] of this.users.entries()) {
      if (user.email === email) return user;
    }
    return null;
  }

  // Architectural issue: tight coupling
  sendNotification(id: string, message: string) {
    const user = this.users.get(id);
    if (user) {
      // Hardcoded dependency
      const mailer = new (require('./mailer'))();
      mailer.send(user.email, message);
    }
  }
}
