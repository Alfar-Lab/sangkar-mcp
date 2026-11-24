class Worker
  def initialize(name)
    @name = name
  end

  def run(task)
    normalize(task)
  end
end

class FastWorker < Worker
  def run(task)
    super(task) + 1
  end
end

def normalize(task)
  task * 2
end
