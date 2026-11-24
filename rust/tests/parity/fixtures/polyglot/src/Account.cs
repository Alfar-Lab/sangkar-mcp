namespace Polyglot
{
    public class Account
    {
        public int Balance { get; set; }

        public virtual int Deposit(int amount)
        {
            return Adjust(amount);
        }

        protected int Adjust(int amount)
        {
            return amount;
        }
    }

    public class SavingsAccount : Account
    {
        public override int Deposit(int amount)
        {
            return base.Deposit(amount) + 1;
        }
    }
}
