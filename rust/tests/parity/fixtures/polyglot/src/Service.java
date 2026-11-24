package polyglot;

public class Service {
    public String handle(String request) {
        return respond(request);
    }

    protected String respond(String request) {
        return request + "-ok";
    }
}

class CachedService extends Service {
    @Override
    public String handle(String request) {
        return super.handle(request) + "!";
    }
}
